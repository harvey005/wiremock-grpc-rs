use prost::{bytes::BufMut, Message};
use std::{
    net::{SocketAddr, TcpStream},
    sync::{Arc, RwLock},
    task::Poll,
    time::Duration,
};
use tonic::{
    codec::Codec,
    codegen::{http, Body, Never, StdError},
    Code,
};

#[derive(Clone)]
pub struct MockGrpcServer {
    address: SocketAddr,
    inner: Arc<Option<Inner>>,
    rules: Arc<RwLock<Vec<RequestBuilder>>>,
}

struct Inner {
    #[allow(dead_code)]
    join_handle: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}

impl MockGrpcServer {
    pub fn new(port: u16) -> Self {
        Self {
            address: format!("[::1]:{}", port).parse().unwrap(),
            inner: Arc::default(),
            rules: Arc::default(),
        }
    }

    pub async fn start_default() -> Self {
        let port = MockGrpcServer::find_unused_port()
            .await
            .expect("Unable to find an open port");

        MockGrpcServer::new(port).start().await
    }

    async fn find_unused_port() -> Option<u16> {
        for port in 50000..60000 {
            let addr: SocketAddr = format!("[::1]:{}", port).parse().unwrap();

            if !TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(25)).is_ok() {
                return Some(port);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        None
    }

    pub async fn start(mut self) -> Self {
        println!("Starting gRPC started in {}", self.address());

        let thread = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(self.clone())
                .serve(self.address),
        );

        for _ in 0..40 {
            if TcpStream::connect_timeout(&self.address, std::time::Duration::from_millis(25))
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        self.inner = Arc::new(Some(Inner {
            join_handle: thread,
        }));

        println!("Server started in {}", self.address());
        self
    }

    pub fn setup(&mut self, r: RequestBuilder) -> MockGrpcServer {
        r.mount(self);

        self.to_owned()
    }

    pub fn address(&self) -> &SocketAddr {
        &self.address
    }
}

impl tonic::transport::NamedService for MockGrpcServer {
    const NAME: &'static str = "hello.Greeter";
}

impl<B> tonic::codegen::Service<http::Request<B>> for MockGrpcServer
where
    B: Body + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    type Response = http::Response<tonic::body::BoxBody>;
    type Error = Never;
    type Future = tonic::codegen::BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        println!("Request to {}", req.uri().path());

        let builder = http::Response::builder()
            .status(200)
            .header("content-type", "application/grpc");

        let path = req.uri().path();
        let inner = self.rules.as_ref();
        let inner = inner.read().unwrap();

        if let Some(req_builder) = inner.iter().find(|x| x.path == path) {
            println!("Matched rule {:?}", req_builder);
            let status = req_builder.status_code.unwrap_or(Code::Ok) as u32;
            println!("Setting status: {}", status);
            let builder = builder.header("grpc-status", format!("{}", status));

            if let Some(body) = &req_builder.result {
                println!("Returning body ({} bytes)", body.len());
                let body = body.clone();

                let fut = async move {
                    let method = SvcGeneric(body);
                    let codec = GenericCodec::default();

                    let mut grpc = tonic::server::Grpc::new(codec);
                    let res = grpc.unary(method, req).await;

                    Ok(res)
                };
                return Box::pin(fut);
            } else {
                println!("Returning empty body");

                return Box::pin(async move {
                    let body = builder.body(tonic::body::empty_body()).unwrap();
                    Ok(body)
                });
            };
        }

        println!("Request unhandled");
        panic!("Mock is not setup for {}", path);
    }
}

struct SvcGeneric(Vec<u8>);
impl tonic::server::UnaryService<Vec<u8>> for SvcGeneric {
    type Response = Vec<u8>;
    type Future = tonic::codegen::BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
    fn call(&mut self, _: tonic::Request<Vec<u8>>) -> Self::Future {
        let body = self.0.clone();
        let fut = async move { Ok(tonic::Response::new(body)) };

        Box::pin(fut)
    }
}

struct GenericCodec;

impl Default for GenericCodec {
    fn default() -> Self {
        Self {}
    }
}

impl Codec for GenericCodec {
    type Encode = Vec<u8>;
    type Decode = Vec<u8>;

    type Encoder = GenericProstEncoder;
    type Decoder = GenericProstDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        GenericProstEncoder(Vec::default())
    }

    fn decoder(&mut self) -> Self::Decoder {
        GenericProstDecoder {}
    }
}

/// A [`Encoder`] that knows how to encode `T`.
#[derive(Debug, Clone, Default)]
pub struct GenericProstEncoder(Vec<u8>);

impl tonic::codec::Encoder for GenericProstEncoder {
    type Item = Vec<u8>;
    type Error = tonic::Status;

    fn encode(
        &mut self,
        item: Self::Item,
        buf: &mut tonic::codec::EncodeBuf<'_>,
    ) -> Result<(), Self::Error> {
        // construct the BytesMut from the Vec<u8>
        let mut b = prost::bytes::BytesMut::new();
        for i in item {
            b.put_u8(i);
        }

        // copy to buffer
        for i in b {
            buf.put_u8(i);
        }

        Ok(())
    }
}

/// A [`Decoder`] that knows how to decode `U`.
#[derive(Debug, Clone, Default)]
pub struct GenericProstDecoder;

impl tonic::codec::Decoder for GenericProstDecoder {
    type Item = Vec<u8>;
    type Error = tonic::Status;

    fn decode(
        &mut self,
        buf: &mut tonic::codec::DecodeBuf<'_>,
    ) -> Result<Option<Self::Item>, Self::Error> {
        let item = Message::decode(buf)
            .map(Option::Some)
            .map_err(from_decode_error)?;

        Ok(item)
    }
}

fn from_decode_error(error: prost::DecodeError) -> tonic::Status {
    // Map Protobuf parse errors to an INTERNAL status code, as per
    // https://github.com/grpc/grpc/blob/master/doc/statuscodes.md
    tonic::Status::new(Code::Internal, error.to_string())
}

#[derive(Debug)]
pub struct RequestBuilder {
    path: String,
    status_code: Option<tonic::Code>,
    result: Option<Vec<u8>>,
}

impl RequestBuilder {
    pub fn given(path: &str) -> Self {
        Self {
            path: path.into(),
            result: None,
            status_code: None,
        }
    }

    pub fn when(&self) -> Self {
        todo!()
    }

    pub fn return_status(self, status: tonic::Code) -> Self {
        Self {
            status_code: Some(status),
            ..self
        }
    }

    pub fn return_body<T, F>(self, f: F) -> Self
    where
        F: Fn() -> T,
        T: prost::Message,
    {
        let result = f();
        let mut buf = prost::bytes::BytesMut::new();
        let _ = result
            .encode(&mut buf)
            .expect("Unable to encode the message");
        let result = buf.to_vec();

        Self {
            result: Some(result),
            ..self
        }
    }

    pub fn mount(self, s: &mut MockGrpcServer) {
        if self.status_code.is_none() && self.result.is_none() {
            panic!("Must set the status code or body before attempting to mount the rule.");
        }

        s.rules.write().unwrap().push(self);
    }
}
