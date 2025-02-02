//! A lightweight asynchronous [smux](https://github.com/xtaci/smux) (Simple MUltipleXing) library for smol, async-std and any async runtime compatible to `futures`.

pub mod builder;
pub mod config;
pub mod error;
pub(crate) mod frame;
pub(crate) mod mux;

pub use builder::MuxBuilder;
pub use config::{MuxConfig, StreamIdType};
pub use mux::{mux_connection, MuxAcceptor, MuxConnector, MuxStream};

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU64, time::Duration};

    use rand::RngCore;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    use crate::{builder::MuxBuilder, frame::MAX_PAYLOAD_SIZE, mux::TokioConn, MuxStream};

    async fn get_tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let h = tokio::spawn(async move {
            let (a, _) = listener.accept().await.unwrap();
            a
        });

        let b = TcpStream::connect(addr).await.unwrap();
        let a = h.await.unwrap();
        a.set_nodelay(true).unwrap();
        b.set_nodelay(true).unwrap();
        (a, b)
    }

    async fn test_stream<T: TokioConn>(mut a: MuxStream<T>, mut b: MuxStream<T>) {
        const LEN: usize = MAX_PAYLOAD_SIZE + 0x200;
        let mut data1 = vec![0; LEN];
        let mut data2 = vec![0; LEN];
        rand::thread_rng().fill_bytes(&mut data1);
        rand::thread_rng().fill_bytes(&mut data2);

        let mut buf = vec![0; LEN];

        a.write_all(&data1).await.unwrap();
        a.flush().await.unwrap();
        b.write_all(&data2).await.unwrap();
        b.flush().await.unwrap();

        a.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, data2);
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, data1);

        a.write_all(&data1).await.unwrap();
        a.flush().await.unwrap();
        b.read_exact(&mut buf[..LEN / 2]).await.unwrap();
        b.read_exact(&mut buf[LEN / 2..]).await.unwrap();
        assert_eq!(buf, data1);

        a.write_all(&data1[..LEN / 2]).await.unwrap();
        a.flush().await.unwrap();
        b.read_exact(&mut buf[..LEN / 2]).await.unwrap();
        assert_eq!(buf[..LEN / 2], data1[..LEN / 2]);

        a.shutdown().await.unwrap();
        b.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_tcp() {
        let (a, b) = get_tcp_pair().await;
        let (connector_a, mut acceptor_a, worker_a) =
            MuxBuilder::client().with_connection(a).build();
        let (connector_b, mut acceptor_b, worker_b) =
            MuxBuilder::server().with_connection(b).build();
        tokio::spawn(async move {
            worker_a.await.unwrap();
        });
        tokio::spawn(async move {
            worker_b.await.unwrap();
        });

        let stream1 = connector_a.connect().unwrap();
        let stream2 = acceptor_b.accept().await.unwrap();
        test_stream(stream1, stream2).await;

        let stream1 = connector_b.connect().unwrap();
        let stream2 = acceptor_a.accept().await.unwrap();
        test_stream(stream1, stream2).await;

        assert_eq!(connector_a.get_num_streams(), 0);
        assert_eq!(connector_b.get_num_streams(), 0);
        assert_eq!(acceptor_a.get_num_streams(), 0);
        assert_eq!(acceptor_b.get_num_streams(), 0);

        let mut streams1 = vec![];
        let mut streams2 = vec![];
        const STREAM_NUM: usize = 0x1000;
        for _ in 0..STREAM_NUM {
            let stream = connector_a.connect().unwrap();
            streams1.push(stream);
        }
        for _ in 0..STREAM_NUM {
            let stream = acceptor_b.accept().await.unwrap();
            streams2.push(stream);
        }

        let handles = streams1
            .into_iter()
            .zip(streams2.into_iter())
            .map(|(a, b)| {
                tokio::spawn(async move {
                    test_stream(a, b).await;
                })
            })
            .collect::<Vec<_>>();

        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(connector_a.get_num_streams(), 0);
        assert_eq!(connector_b.get_num_streams(), 0);
        assert_eq!(acceptor_a.get_num_streams(), 0);
        assert_eq!(acceptor_b.get_num_streams(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_worker_drop() {
        let (a, b) = get_tcp_pair().await;
        let (connector_a, mut acceptor_a, worker_a) =
            MuxBuilder::client().with_connection(a).build();
        let (connector_b, mut acceptor_b, worker_b) =
            MuxBuilder::server().with_connection(b).build();
        let mut stream1 = connector_a.connect().unwrap();
        let h1 = tokio::spawn(async move {
            let mut buf = vec![0; 0x100];
            stream1.read_exact(&mut buf).await.unwrap_err();
        });

        drop(worker_a);
        drop(worker_b);

        assert!(connector_a.connect().is_err());
        assert!(connector_b.connect().is_err());
        assert!(acceptor_a.accept().await.is_none());
        assert!(acceptor_b.accept().await.is_none());
        h1.await.unwrap();
    }

    #[tokio::test]
    async fn test_shutdown() {
        let (a, b) = get_tcp_pair().await;
        let (connector_a, acceptor_a, worker_a) = MuxBuilder::client().with_connection(a).build();
        let (connector_b, mut acceptor_b, worker_b) =
            MuxBuilder::server().with_connection(b).build();
        tokio::spawn(async move {
            worker_a.await.unwrap();
        });
        tokio::spawn(async move {
            worker_b.await.unwrap();
        });

        let mut stream1 = connector_a.connect().unwrap();
        let mut stream2 = acceptor_b.accept().await.unwrap();

        let data = [1, 2, 3, 4];
        stream2.write_all(&data).await.unwrap();
        stream2.shutdown().await.unwrap();

        tokio::time::sleep(Duration::from_secs(1)).await;

        stream1.write_all(&[0, 1, 2, 3]).await.unwrap_err();
        stream1.flush().await.unwrap_err();
        let mut buf = vec![0; 4];
        stream1.read_exact(&mut buf).await.unwrap();
        assert!(buf == data);
        stream1.read(&mut buf).await.unwrap_err();

        drop(acceptor_a);
        let mut stream = connector_b.connect().unwrap();
        stream.read(&mut buf).await.unwrap_err();
        stream.flush().await.unwrap_err();
        stream.shutdown().await.unwrap();

        let mut stream1 = connector_a.connect().unwrap();
        let mut stream2 = acceptor_b.accept().await.unwrap();
        stream1.write_all(&data).await.unwrap();
        stream1.flush().await.unwrap();
        drop(stream1);
        tokio::time::sleep(Duration::from_secs(1)).await;

        let mut buf = vec![0; 4];
        stream2.read_exact(&mut buf).await.unwrap();
        assert!(buf == data);
        stream2.read_exact(&mut buf).await.unwrap_err();
        stream2.write_all(&data).await.unwrap_err();
    }

    #[tokio::test]
    async fn test_timeout() {
        let (a, b) = get_tcp_pair().await;
        let (connector_a, _, worker_a) = MuxBuilder::client()
            .with_idle_timeout(NonZeroU64::new(3).unwrap())
            .with_connection(a)
            .build();
        let (_, mut acceptor_b, worker_b) = MuxBuilder::server().with_connection(b).build();
        tokio::spawn(async move {
            worker_a.await.unwrap();
        });
        tokio::spawn(async move {
            worker_b.await.unwrap();
        });

        let mut stream1 = connector_a.connect().unwrap();
        let mut stream2 = acceptor_b.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!stream1.is_closed());
        assert!(!stream2.is_closed());

        tokio::time::sleep(Duration::from_secs(5)).await;

        assert!(stream1.is_closed());
        assert!(stream2.is_closed());
    }
}
