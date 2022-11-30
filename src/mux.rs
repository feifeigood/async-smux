use std::{
    collections::{HashMap, VecDeque},
    io::ErrorKind,
    num::Wrapping,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll, Waker},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::{Buf, Bytes};
use futures::{ready, Future, FutureExt, SinkExt, Stream, StreamExt};
use futures_sink::Sink;
use parking_lot::Mutex;
use std::io as StdIo;
use tokio::{
    io::{self, AsyncRead, AsyncWrite},
    macros::support::poll_fn,
    time::{interval, Interval},
};
use tokio_util::codec::Framed;

use crate::{
    config::{MuxConfig, StreamIdType},
    error::{MuxError, MuxResult},
    frame::{MuxCodec, MuxCommand, MuxFrame, MAX_PAYLOAD_SIZE},
};

pub trait TokioConn: AsyncRead + AsyncWrite + Unpin {}

impl<T> TokioConn for T where T: AsyncRead + AsyncWrite + Unpin {}

pub fn mux_connection<T: TokioConn>(
    connection: T,
    config: MuxConfig,
) -> (MuxConnector<T>, MuxAcceptor<T>, MuxWorker<T>) {
    let timestamp = Arc::new(AtomicU64::new(get_timestamp_slow()));
    let inner = Framed::new(connection, MuxCodec {});
    let state = Arc::new(Mutex::new(MuxState {
        inner,
        handles: HashMap::new(),
        accept_queue: VecDeque::new(),
        accept_waker: None,
        accpet_closed: false,
        tx_queue: VecDeque::new(),
        tx_waker: None,
        tx_closed: false,
        rx_closed: false,
        stream_id_hint: Wrapping(config.stream_id_type as u32),
        stream_id_type: config.stream_id_type,
        timestamp: timestamp.clone(),
    }));
    (
        MuxConnector {
            state: state.clone(),
        },
        MuxAcceptor {
            state: state.clone(),
        },
        MuxWorker {
            dispatcher: MuxDispatcher {
                state: state.clone(),
            },
            sender: MuxSender {
                state: state.clone(),
            },
            timer: MuxTimer {
                state,
                interval: interval(Duration::from_millis(500)),
                timestamp,
                last_ping: get_timestamp_slow(),
                keep_alive_interval: config.keep_alive_interval.map(|a| a.get()),
                idle_timeout: config.idle_timeout.map(|a| a.get()),
            },
        },
    )
}

#[derive(Clone)]
pub struct MuxConnector<T: TokioConn> {
    state: Arc<Mutex<MuxState<T>>>,
}

impl<T: TokioConn> MuxConnector<T> {
    pub fn connect(&self) -> MuxResult<MuxStream<T>> {
        let mut state = self.state.lock();
        state.check_tx_closed()?;

        let stream_id = state.alloc_stream_id()?;
        state.process_sync(stream_id, false)?;
        let frame = MuxFrame::new(MuxCommand::Sync, stream_id, Bytes::new());
        state.enqueue_frame_global(frame);
        state.wake_tx();

        let stream = MuxStream {
            stream_id,
            state: self.state.clone(),
            read_buffer: None,
        };
        Ok(stream)
    }

    pub fn get_num_streams(&self) -> usize {
        self.state.lock().handles.len()
    }
}

pub struct MuxAcceptor<T: TokioConn> {
    state: Arc<Mutex<MuxState<T>>>,
}

impl<T: TokioConn> Drop for MuxAcceptor<T> {
    fn drop(&mut self) {
        self.state.lock().accpet_closed = true;
    }
}

impl<T: TokioConn> Stream for MuxAcceptor<T> {
    type Item = MuxStream<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut state = self.state.lock();
        if state.check_rx_closed().is_err() {
            return Poll::Ready(None);
        }

        if let Some(stream) = state.accept_queue.pop_front() {
            Poll::Ready(Some(stream))
        } else {
            state.register_accept_waker(cx);
            Poll::Pending
        }
    }
}

impl<T: TokioConn> MuxAcceptor<T> {
    pub async fn accept(&mut self) -> Option<MuxStream<T>> {
        self.next().await
    }

    pub fn get_num_streams(&mut self) -> usize {
        self.state.lock().handles.len()
    }

    pub async fn close(&mut self) -> MuxResult<()> {
        poll_fn(|cx| {
            let mut state = self.state.lock();
            state.close_tx();
            state.close_rx();
            state.inner.poll_close_unpin(cx)
        })
        .await?;
        Ok(())
    }
}

#[inline]
fn get_timestamp_slow() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

struct MuxTimer<T: TokioConn> {
    state: Arc<Mutex<MuxState<T>>>,
    interval: Interval,
    timestamp: Arc<AtomicU64>,

    last_ping: u64,
    keep_alive_interval: Option<u64>,

    idle_timeout: Option<u64>,
}

impl<T: TokioConn> Future for MuxTimer<T> {
    type Output = MuxResult<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            ready!(self.interval.poll_tick(cx));
            self.interval.reset();

            let mut ping_processed = false;
            let ts = get_timestamp_slow();
            self.timestamp.store(ts, Ordering::SeqCst);
            let mut state = self.state.lock();

            // Ping
            if let Some(keep_alive_interval) = self.keep_alive_interval {
                if ts > self.last_ping + keep_alive_interval {
                    ping_processed = true;
                    state.enqueue_frame_global(MuxFrame::new(MuxCommand::Nop, 0, Bytes::new()));
                    state.wake_tx();
                }
            }

            // Clean timeout
            if let Some(idle_timeout) = self.idle_timeout {
                let dead_ids = state
                    .handles
                    .iter()
                    .filter_map(|(id, h)| {
                        if ts > h.last_active + idle_timeout {
                            Some(*id)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();

                for stream_id in dead_ids {
                    state.process_finish(stream_id);
                    state.send_finish(stream_id);
                }
            }

            std::mem::drop(state);
            // update ping interval
            if ping_processed {
                self.last_ping = ts;
            }
        }
    }
}

struct MuxSender<T: TokioConn> {
    state: Arc<Mutex<MuxState<T>>>,
}

impl<T: TokioConn> Future for MuxSender<T> {
    type Output = MuxResult<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let mut state = self.state.lock();
            ready!(state.poll_flush_frames(cx))?;
            ready!(state.poll_flush_inner(cx))?;
            ready!(state.poll_ready_tx(cx))?;
        }
    }
}

impl<T: TokioConn> Drop for MuxSender<T> {
    fn drop(&mut self) {
        self.state.lock().close_tx();
    }
}

struct MuxDispatcher<T: TokioConn> {
    state: Arc<Mutex<MuxState<T>>>,
}

impl<T: TokioConn> Drop for MuxDispatcher<T> {
    fn drop(&mut self) {
        self.state.lock().close_rx();
    }
}

impl<T: TokioConn> Future for MuxDispatcher<T> {
    type Output = MuxResult<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let mut state = self.state.lock();
            if state.rx_closed {
                return Poll::Ready(Ok(()));
            }

            let frame = ready!(state.poll_next_frame(cx))?;
            match frame.header.command {
                MuxCommand::Sync => {
                    if state.accpet_closed {
                        state.send_finish(frame.header.stream_id);
                        continue;
                    }

                    state.process_sync(frame.header.stream_id, true)?;

                    let stream = MuxStream {
                        stream_id: frame.header.stream_id,
                        state: self.state.clone(),
                        read_buffer: None,
                    };
                    state.accept_queue.push_back(stream);
                    state.wake_accept();
                }
                MuxCommand::Finish => {
                    state.process_finish(frame.header.stream_id);
                }
                MuxCommand::Push => {
                    let stream_id = frame.header.stream_id;
                    if !state.process_push(frame) {
                        state.send_finish(stream_id);
                    }
                }
                MuxCommand::Nop => {
                    // Do nothing
                }
            }
        }
    }
}

pub struct MuxWorker<T: TokioConn> {
    dispatcher: MuxDispatcher<T>,
    sender: MuxSender<T>,
    timer: MuxTimer<T>,
}

impl<T: TokioConn> Future for MuxWorker<T> {
    type Output = MuxResult<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.sender.poll_unpin(cx)?.is_ready() {
            return Poll::Ready(Ok(()));
        }
        if self.timer.poll_unpin(cx)?.is_ready() {
            return Poll::Ready(Ok(()));
        }
        if self.dispatcher.poll_unpin(cx)?.is_ready() {
            return Poll::Ready(Ok(()));
        }

        Poll::Pending
    }
}

pub struct MuxStream<T: TokioConn> {
    stream_id: u32,
    state: Arc<Mutex<MuxState<T>>>,
    read_buffer: Option<Bytes>,
}

impl<T: TokioConn> Drop for MuxStream<T> {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        if state.is_open(self.stream_id) {
            // The user did not call `shutdown()`
            state.enqueue_frame_global(MuxFrame::new(
                MuxCommand::Finish,
                self.stream_id,
                Bytes::new(),
            ));
            state.wake_tx();
        }
        state.remove_stream(self.stream_id);
    }
}

impl<T: TokioConn> AsyncRead for MuxStream<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> Poll<StdIo::Result<()>> {
        loop {
            if let Some(read_buffer) = &mut self.read_buffer {
                if read_buffer.len() <= buf.remaining() {
                    buf.put_slice(read_buffer);
                    self.read_buffer = None;
                } else {
                    let len = buf.remaining();
                    buf.put_slice(&read_buffer[..len]);
                    read_buffer.advance(len);
                }
                return Poll::Ready(Ok(()));
            }

            let frame = ready!(self.state.lock().poll_read_stream_data(cx, self.stream_id))
                .map_err(mux_to_io_err)?;
            debug_assert_eq!(frame.header.command, MuxCommand::Push);
            self.read_buffer = Some(frame.payload);
        }
    }
}

fn mux_to_io_err(e: MuxError) -> StdIo::Error {
    StdIo::Error::new(ErrorKind::Other, e)
}

impl<T: TokioConn> AsyncWrite for MuxStream<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, StdIo::Error>> {
        let mut state = self.state.lock();
        if !state.is_open(self.stream_id) {
            return Poll::Ready(Err(StdIo::ErrorKind::ConnectionReset.into()));
        }

        let mut write_buffer = Bytes::copy_from_slice(buf);
        while !write_buffer.is_empty() {
            let len = write_buffer.len().min(MAX_PAYLOAD_SIZE);
            let payload = write_buffer.split_to(len);
            let frame = MuxFrame::new(MuxCommand::Push, self.stream_id, payload);
            state.enqueue_frame_stream(self.stream_id, frame);
        }
        state.wake_tx();
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), StdIo::Error>> {
        let mut state = self.state.lock();
        if !state.is_open(self.stream_id) {
            return Poll::Ready(Err(StdIo::Error::new(
                ErrorKind::ConnectionReset,
                "stream is already closed",
            )));
        }

        state
            .poll_flush_stream_frames(cx, self.stream_id)
            .map_err(mux_to_io_err)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), StdIo::Error>> {
        loop {
            let mut state = self.state.lock();
            state.check_tx_closed().map_err(mux_to_io_err)?;
            ready!(state
                .poll_flush_stream_frames(cx, self.stream_id)
                .map_err(mux_to_io_err))?;
            if !state.is_open(self.stream_id) {
                return Poll::Ready(Ok(()));
            }

            state.process_finish(self.stream_id);
            state.send_finish(self.stream_id);
        }
    }
}

impl<T: TokioConn> MuxStream<T> {
    pub fn is_closed(&mut self) -> bool {
        !self.state.lock().is_open(self.stream_id)
    }
}

struct StreamHandle {
    closed: bool,

    tx_queue: VecDeque<MuxFrame>,
    tx_waker: Option<Waker>,

    rx_queue: VecDeque<MuxFrame>,
    rx_waker: Option<Waker>,

    last_active: u64,
}

impl StreamHandle {
    fn new(ts: u64) -> Self {
        Self {
            closed: false,
            tx_queue: VecDeque::with_capacity(128),
            tx_waker: None,
            rx_queue: VecDeque::with_capacity(128),
            rx_waker: None,
            last_active: ts,
        }
    }

    #[inline]
    fn register_tx_waker(&mut self, cx: &Context<'_>) {
        self.tx_waker = Some(cx.waker().clone());
    }

    #[inline]
    fn register_rx_waker(&mut self, cx: &Context<'_>) {
        self.rx_waker = Some(cx.waker().clone());
    }

    #[inline]
    fn wake_rx(&mut self) {
        if let Some(waker) = self.rx_waker.take() {
            waker.wake();
        }
    }

    #[inline]
    fn wake_tx(&mut self) {
        if let Some(waker) = self.tx_waker.take() {
            waker.wake();
        }
    }
}

struct MuxState<T: TokioConn> {
    inner: Framed<T, MuxCodec>,
    handles: HashMap<u32, StreamHandle>,

    accept_queue: VecDeque<MuxStream<T>>,
    accept_waker: Option<Waker>,
    accpet_closed: bool,

    tx_queue: VecDeque<MuxFrame>,
    tx_waker: Option<Waker>,

    tx_closed: bool,
    rx_closed: bool,

    stream_id_hint: Wrapping<u32>,
    stream_id_type: StreamIdType,

    timestamp: Arc<AtomicU64>,
}

impl<T: TokioConn> MuxState<T> {
    fn alloc_stream_id(&mut self) -> MuxResult<u32> {
        if self.handles.len() >= (u32::MAX / 2) as usize {
            return Err(MuxError::TooManyStreams);
        }

        while self.handles.contains_key(&self.stream_id_hint.0) {
            self.stream_id_hint += 2;
        }

        Ok(self.stream_id_hint.0)
    }

    fn remove_stream(&mut self, stream_id: u32) {
        self.handles
            .remove(&stream_id)
            .expect("Failed to remove stream");
    }

    fn send_finish(&mut self, stream_id: u32) {
        self.enqueue_frame_global(MuxFrame::new(MuxCommand::Finish, stream_id, Bytes::new()));
        self.wake_tx();
    }

    fn process_sync(&mut self, stream_id: u32, from_peer: bool) -> MuxResult<()> {
        if self.handles.contains_key(&stream_id) {
            return Err(MuxError::DuplicatedStreamId(stream_id));
        }

        if (stream_id % 2 != self.stream_id_type as _) ^ from_peer {
            return Err(MuxError::InvalidPeerStreamIdType(
                stream_id,
                self.stream_id_type,
            ));
        }

        let handle = StreamHandle::new(self.get_timestamp());
        self.handles.insert(stream_id, handle);
        Ok(())
    }

    fn process_finish(&mut self, stream_id: u32) {
        if let Some(handle) = self.handles.get_mut(&stream_id) {
            handle.closed = true;
            handle.wake_tx();
            handle.wake_rx();
        }
    }

    fn process_push(&mut self, frame: MuxFrame) -> bool {
        let ts = self.get_timestamp();
        if let Some(handle) = self.handles.get_mut(&frame.header.stream_id) {
            handle.rx_queue.push_back(frame);
            handle.wake_rx();
            handle.last_active = ts;
            true
        } else {
            false
        }
    }

    fn is_open(&self, stream_id: u32) -> bool {
        if let Some(handle) = self.handles.get(&stream_id) {
            !handle.closed
        } else {
            false
        }
    }

    fn poll_next_frame(&mut self, cx: &mut Context<'_>) -> Poll<MuxResult<MuxFrame>> {
        if self.rx_closed {
            return Poll::Ready(Err(MuxError::ConnectionClosed));
        }

        if let Some(r) = ready!(self.inner.poll_next_unpin(cx)) {
            let frame = r.map_err(|e| {
                self.rx_closed = true;
                e
            })?;
            Poll::Ready(Ok(frame))
        } else {
            self.rx_closed = true;
            Poll::Ready(Err(MuxError::ConnectionClosed))
        }
    }

    #[inline]
    fn pin_inner(&mut self) -> Pin<&mut Framed<T, MuxCodec>> {
        Pin::new(&mut self.inner)
    }

    fn poll_write_ready(&mut self, cx: &mut Context<'_>) -> Poll<MuxResult<()>> {
        ready!(self.pin_inner().poll_ready(cx)).map_err(|e| {
            self.tx_closed = true;
            e
        })?;
        Poll::Ready(Ok(()))
    }

    fn write_frame(&mut self, frame: MuxFrame) -> MuxResult<()> {
        self.pin_inner().start_send(frame)?;
        Ok(())
    }

    fn poll_read_stream_data(
        &mut self,
        cx: &mut Context<'_>,
        stream_id: u32,
    ) -> Poll<MuxResult<MuxFrame>> {
        let handle = self.handles.get_mut(&stream_id).unwrap();
        if let Some(f) = handle.rx_queue.pop_front() {
            Poll::Ready(Ok(f))
        } else if handle.closed {
            Poll::Ready(Err(MuxError::StreamClosed(stream_id)))
        } else {
            handle.register_rx_waker(cx);
            Poll::Pending
        }
    }

    #[inline]
    fn get_timestamp(&self) -> u64 {
        self.timestamp.load(Ordering::Relaxed)
    }

    fn enqueue_frame_stream(&mut self, stream_id: u32, frame: MuxFrame) {
        let ts = self.get_timestamp();
        let handle = self.handles.get_mut(&stream_id).unwrap();
        handle.tx_queue.push_back(frame);
        handle.last_active = ts;
    }

    #[inline]
    fn enqueue_frame_global(&mut self, frame: MuxFrame) {
        self.tx_queue.push_back(frame);
    }

    #[inline]
    fn register_tx_waker(&mut self, cx: &Context<'_>) {
        self.tx_waker = Some(cx.waker().clone());
    }

    #[inline]
    fn wake_tx(&mut self) {
        if let Some(waker) = self.tx_waker.take() {
            waker.wake();
        }
    }

    fn poll_flush_stream_frames(
        &mut self,
        cx: &mut Context<'_>,
        stream_id: u32,
    ) -> Poll<MuxResult<()>> {
        let handle = self.handles.get_mut(&stream_id).unwrap();
        if handle.tx_queue.is_empty() {
            Poll::Ready(Ok(()))
        } else {
            handle.register_tx_waker(cx);
            self.wake_tx();
            Poll::Pending
        }
    }

    #[inline]
    fn register_accept_waker(&mut self, cx: &Context<'_>) {
        self.accept_waker = Some(cx.waker().clone());
    }

    #[inline]
    fn wake_accept(&mut self) {
        if let Some(waker) = self.accept_waker.take() {
            waker.wake();
        }
    }

    fn close_tx(&mut self) {
        self.tx_closed = true;
        for (_, handle) in self.handles.iter_mut() {
            handle.closed = true;
            handle.wake_tx();
        }
    }

    fn close_rx(&mut self) {
        self.rx_closed = true;
        self.wake_accept();
        for (_, handle) in self.handles.iter_mut() {
            handle.closed = true;
            handle.wake_rx();
        }
    }

    fn check_rx_closed(&self) -> MuxResult<()> {
        if self.tx_closed {
            Err(MuxError::ConnectionClosed)
        } else {
            Ok(())
        }
    }

    fn check_tx_closed(&self) -> MuxResult<()> {
        if self.tx_closed {
            Err(MuxError::ConnectionClosed)
        } else {
            Ok(())
        }
    }

    fn poll_flush_frames(&mut self, cx: &mut Context<'_>) -> Poll<MuxResult<()>> {
        // Global
        while !self.tx_queue.is_empty() {
            ready!(self.poll_write_ready(cx))?;
            let frame = self.tx_queue.pop_front().unwrap();
            self.write_frame(frame)?;
        }

        // Streams
        for (_, h) in self
            .handles
            .iter_mut()
            .filter(|(_, h)| !h.tx_queue.is_empty())
        {
            while !h.tx_queue.is_empty() {
                ready!(Pin::new(&mut self.inner).poll_ready(cx))?;
                Pin::new(&mut self.inner).start_send(h.tx_queue.pop_front().unwrap())?;
            }
            h.wake_tx();
        }

        Poll::Ready(Ok(()))
    }

    fn poll_flush_inner(&mut self, cx: &mut Context<'_>) -> Poll<MuxResult<()>> {
        self.inner.poll_flush_unpin(cx)
    }

    fn poll_ready_tx(&mut self, cx: &mut Context<'_>) -> Poll<MuxResult<()>> {
        if self.tx_closed {
            return Poll::Ready(Err(MuxError::ConnectionClosed));
        }

        if self.tx_queue.is_empty() && self.handles.iter().all(|(_, h)| h.tx_queue.is_empty()) {
            self.register_tx_waker(cx);
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
}
