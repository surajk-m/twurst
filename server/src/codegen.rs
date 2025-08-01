use crate::TwirpError;
use axum::RequestExt;
pub use axum::Router;
use axum::body::Body;
pub use axum::extract::FromRequestParts;
use axum::extract::{Request, State};
#[cfg(feature = "grpc")]
use axum::http::Method;
use axum::http::header::CONTENT_TYPE;
pub use axum::http::request::Parts as RequestParts;
use axum::http::{HeaderMap, HeaderValue};
pub use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::post;
use http_body_util::BodyExt;
#[cfg(feature = "grpc")]
use pin_project_lite::pin_project;
use prost_reflect::bytes::{Bytes, BytesMut};
use prost_reflect::{DynamicMessage, ReflectMessage};
use serde::Serialize;
use std::future::Future;
#[cfg(feature = "grpc")]
use std::pin::Pin;
#[cfg(feature = "grpc")]
use std::task::{Context, Poll};
#[cfg(feature = "grpc")]
pub use tokio_stream::Stream;
#[cfg(feature = "grpc")]
use tokio_stream::StreamExt;
use tracing::error;
pub use trait_variant::make as trait_variant_make;
use twurst_error::TwirpErrorCode;

const APPLICATION_JSON: HeaderValue = HeaderValue::from_static("application/json");
const APPLICATION_PROTOBUF: HeaderValue = HeaderValue::from_static("application/protobuf");

pub struct TwirpRouter<S, RS = ()> {
    router: Router<RS>,
    service: S,
}

impl<S: Clone + Send + Sync + 'static, RS: Clone + Send + Sync + 'static> TwirpRouter<S, RS> {
    pub fn new(service: S) -> Self {
        Self {
            router: Router::new(),
            service,
        }
    }

    pub fn route<
        I: ReflectMessage + Default,
        O: ReflectMessage,
        F: Future<Output = Result<O, TwirpError>> + Send,
    >(
        mut self,
        path: &str,
        call: impl (Fn(S, I, RequestParts, RS) -> F) + Clone + Send + Sync + 'static,
    ) -> Self {
        let service = self.service.clone();
        self.router = self.router.route(
            path,
            post(
                move |State(state): State<RS>, request: Request| async move {
                    let (parts, body) = request.with_limited_body().into_parts();
                    let content_type = ContentType::from_headers(&parts.headers)?;
                    let request = parse_request(content_type, body).await?;
                    let response = call(service, request, parts, state).await?;
                    serialize_response(content_type, response)
                },
            ),
        );
        self
    }

    pub fn route_streaming(mut self, path: &str) -> Self {
        self.router = self.router.route(
            path,
            post(move || async move {
                TwirpError::unimplemented("Streaming is not supported by Twirp")
            }),
        );
        self
    }

    pub fn build(self) -> Router<RS> {
        self.router
    }
}

#[derive(Clone, Copy)]
enum ContentType {
    Protobuf,
    Json,
}

impl ContentType {
    fn from_headers(headers: &HeaderMap) -> Result<Self, TwirpError> {
        let content_type = headers
            .get(CONTENT_TYPE)
            .ok_or_else(|| TwirpError::malformed("No content-type header"))?;
        if content_type == APPLICATION_PROTOBUF {
            Ok(ContentType::Protobuf)
        } else if content_type == APPLICATION_JSON {
            Ok(ContentType::Json)
        } else {
            Err(TwirpError::malformed(format!(
                "Unsupported content type: {}",
                String::from_utf8_lossy(content_type.as_bytes())
            )))
        }
    }
}

async fn parse_request<I: ReflectMessage + Default>(
    content_type: ContentType,
    body: Body,
) -> Result<I, TwirpError> {
    let body = body.collect().await.map_err(|e| {
        TwirpError::wrap(
            TwirpErrorCode::Internal,
            "Failed to read the request body",
            e,
        )
    })?;
    match content_type {
        ContentType::Protobuf => I::decode(body.aggregate()).map_err(|e| {
            TwirpError::wrap(
                TwirpErrorCode::Malformed,
                format!("Invalid binary protobuf request: {e}"),
                e,
            )
        }),
        ContentType::Json => json_decode(&body.to_bytes()), // TODO: avoid to_bytes?
    }
}

fn serialize_response<O: ReflectMessage>(
    content_type: ContentType,
    response: O,
) -> Result<Response, TwirpError> {
    let (content_type, body) = match content_type {
        ContentType::Protobuf => {
            let mut buffer = BytesMut::with_capacity(response.encoded_len());
            response.encode(&mut buffer).map_err(|e| {
                TwirpError::wrap(
                    TwirpErrorCode::Internal,
                    format!("Failed to serialize to protobuf: {e}"),
                    e,
                )
            })?;
            (APPLICATION_PROTOBUF, buffer.into())
        }
        ContentType::Json => (APPLICATION_JSON, json_encode(&response)?),
    };
    Response::builder()
        .header(CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .map_err(|e| {
            error!("Failed to build the response: {e}");
            TwirpError::internal("Failed to build the response")
        })
}

fn json_encode<T: ReflectMessage>(message: &T) -> Result<Bytes, TwirpError> {
    let mut serializer = serde_json::Serializer::new(Vec::new());
    message
        .transcode_to_dynamic()
        .serialize(&mut serializer)
        .map_err(|e| {
            error!("Failed to serialize the JSON response: {e}");
            TwirpError::internal("Failed to build the response")
        })?;
    Ok(serializer.into_inner().into())
}

fn json_decode<T: ReflectMessage + Default>(message: &[u8]) -> Result<T, TwirpError> {
    let dynamic_message = dynamic_json_decode::<T>(message).map_err(|e| {
        TwirpError::wrap(
            TwirpErrorCode::Malformed,
            format!("Invalid JSON protobuf request: {e}"),
            e,
        )
    })?;
    dynamic_message.transcode_to().map_err(|e| {
        error!("Failed to cast input message: {e}");
        TwirpError::internal("Internal error while parsing the JSON request")
    })
}

fn dynamic_json_decode<T: ReflectMessage + Default>(
    message: &[u8],
) -> Result<DynamicMessage, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_slice(message);
    let dynamic_message =
        DynamicMessage::deserialize(T::default().descriptor(), &mut deserializer)?;
    deserializer.end()?;
    Ok(dynamic_message)
}

#[cfg(feature = "grpc")]
pub struct GrpcRouter<S> {
    router: Router,
    service: S,
}

#[cfg(feature = "grpc")]
impl<S: Clone + Send + Sync + 'static> GrpcRouter<S> {
    pub fn new(service: S) -> Self {
        Self {
            router: Router::new(),
            service,
        }
    }

    pub fn route<
        I: ReflectMessage + Default + 'static,
        O: ReflectMessage + 'static,
        C: (Fn(S, I, RequestParts) -> F) + Clone + Send + Sync + 'static,
        F: Future<Output = Result<O, TwirpError>> + Send + 'static,
    >(
        mut self,
        path: &str,
        callback: C,
    ) -> Self {
        let service = self.service.clone();
        self.router = self.router.route(
            path,
            post(move |request: Request| async move {
                let method = GrpcService { service, callback };
                let codec = tonic_prost::ProstCodec::default();
                let mut grpc = tonic::server::Grpc::new(codec);
                grpc.unary(method, request).await
            }),
        );
        self
    }

    pub fn route_server_streaming<
        I: ReflectMessage + Default + 'static,
        O: ReflectMessage + 'static,
        C: (Fn(S, I, RequestParts) -> F) + Clone + Send + Sync + 'static,
        F: Future<Output = Result<OS, TwirpError>> + Send + 'static,
        OS: Stream<Item = Result<O, TwirpError>> + Send + 'static,
    >(
        mut self,
        path: &str,
        callback: C,
    ) -> Self {
        let service = self.service.clone();
        self.router = self.router.route(
            path,
            post(move |request: Request| async move {
                let method = GrpcService { service, callback };
                let codec = tonic_prost::ProstCodec::default();
                let mut grpc = tonic::server::Grpc::new(codec);
                grpc.server_streaming(method, request).await
            }),
        );
        self
    }

    pub fn route_client_streaming<
        I: ReflectMessage + Default + 'static,
        O: ReflectMessage + 'static,
        C: (Fn(S, GrpcClientStream<I>, RequestParts) -> F) + Clone + Send + Sync + 'static,
        F: Future<Output = Result<O, TwirpError>> + Send + 'static,
    >(
        mut self,
        path: &str,
        callback: C,
    ) -> Self {
        let service = self.service.clone();
        self.router = self.router.route(
            path,
            post(move |request: Request| async move {
                let method = GrpcService { service, callback };
                let codec = tonic_prost::ProstCodec::default();
                let mut grpc = tonic::server::Grpc::new(codec);
                grpc.client_streaming(method, request).await
            }),
        );
        self
    }

    pub fn route_streaming<
        I: ReflectMessage + Default + 'static,
        O: ReflectMessage + 'static,
        C: (Fn(S, GrpcClientStream<I>, RequestParts) -> F) + Clone + Send + Sync + 'static,
        F: Future<Output = Result<OS, TwirpError>> + Send + 'static,
        OS: Stream<Item = Result<O, TwirpError>> + Send + 'static,
    >(
        mut self,
        path: &str,
        callback: C,
    ) -> Self {
        let service = self.service.clone();
        self.router = self.router.route(
            path,
            post(move |request: Request| async move {
                let method = GrpcService { service, callback };
                let codec = tonic_prost::ProstCodec::default();
                let mut grpc = tonic::server::Grpc::new(codec);
                grpc.streaming(method, request).await
            }),
        );
        self
    }

    pub fn build(self) -> Router {
        self.router
    }
}

#[cfg(feature = "grpc")]
struct GrpcService<S, C> {
    service: S,
    callback: C,
}

#[cfg(feature = "grpc")]
impl<
    S: Clone + Send + Sync + 'static,
    I: ReflectMessage + Default + 'static,
    O: ReflectMessage + 'static,
    C: (Fn(S, I, RequestParts) -> F) + Clone + Send + 'static,
    F: Future<Output = Result<O, TwirpError>> + Send + 'static,
> tonic::server::UnaryService<I> for GrpcService<S, C>
{
    type Response = O;
    type Future = TonicResponseFuture<O>;

    fn call(&mut self, request: tonic::Request<I>) -> Self::Future {
        let (request, parts) = grpc_to_twirp_request(request);
        let result_future = (self.callback)(self.service.clone(), request, parts);
        Box::pin(async move { Ok(tonic::Response::new(result_future.await?)) })
    }
}

#[cfg(feature = "grpc")]
impl<
    S: Clone + Send + Sync + 'static,
    I: ReflectMessage + Default + 'static,
    O: ReflectMessage + 'static,
    C: (Fn(S, I, RequestParts) -> F) + Clone + Send + 'static,
    F: Future<Output = Result<OS, TwirpError>> + Send + 'static,
    OS: Stream<Item = Result<O, TwirpError>> + Send + 'static,
> tonic::server::ServerStreamingService<I> for GrpcService<S, C>
{
    type Response = O;
    type ResponseStream = Pin<Box<dyn Stream<Item = Result<O, tonic::Status>> + Send>>;
    type Future = TonicResponseFuture<Self::ResponseStream>;

    fn call(&mut self, request: tonic::Request<I>) -> Self::Future {
        let (request, parts) = grpc_to_twirp_request(request);
        let result_future = (self.callback)(self.service.clone(), request, parts);
        Box::pin(async move {
            Ok(tonic::Response::new(
                Box::pin(result_future.await?.map(|item| Ok(item?))) as Self::ResponseStream,
            ))
        })
    }
}

#[cfg(feature = "grpc")]
impl<
    S: Clone + Send + Sync + 'static,
    I: ReflectMessage + Default + 'static,
    O: ReflectMessage + 'static,
    C: (Fn(S, GrpcClientStream<I>, RequestParts) -> F) + Clone + Send + 'static,
    F: Future<Output = Result<O, TwirpError>> + Send + 'static,
> tonic::server::ClientStreamingService<I> for GrpcService<S, C>
{
    type Response = O;
    type Future = TonicResponseFuture<Self::Response>;

    fn call(&mut self, request: tonic::Request<tonic::Streaming<I>>) -> Self::Future {
        let (request, parts) = grpc_to_twirp_request(request);
        let request = GrpcClientStream { stream: request };
        let result_future = (self.callback)(self.service.clone(), request, parts);
        Box::pin(async move { Ok(tonic::Response::new(result_future.await?)) })
    }
}

#[cfg(feature = "grpc")]
impl<
    S: Clone + Send + Sync + 'static,
    I: ReflectMessage + Default + 'static,
    O: ReflectMessage + 'static,
    C: (Fn(S, GrpcClientStream<I>, RequestParts) -> F) + Clone + Send + 'static,
    F: Future<Output = Result<OS, TwirpError>> + Send + 'static,
    OS: Stream<Item = Result<O, TwirpError>> + Send + 'static,
> tonic::server::StreamingService<I> for GrpcService<S, C>
{
    type Response = O;
    type ResponseStream = Pin<Box<dyn Stream<Item = Result<O, tonic::Status>> + Send>>;
    type Future = TonicResponseFuture<Self::ResponseStream>;

    fn call(&mut self, request: tonic::Request<tonic::Streaming<I>>) -> Self::Future {
        let (request, parts) = grpc_to_twirp_request(request);
        let request = GrpcClientStream { stream: request };
        let result_future = (self.callback)(self.service.clone(), request, parts);
        Box::pin(async move {
            Ok(tonic::Response::new(
                Box::pin(result_future.await?.map(|item| Ok(item?))) as Self::ResponseStream,
            ))
        })
    }
}

#[cfg(feature = "grpc")]
type TonicResponseFuture<R> =
    Pin<Box<dyn Future<Output = Result<tonic::Response<R>, tonic::Status>> + Send + 'static>>;

#[cfg(feature = "grpc")]
fn grpc_to_twirp_request<T>(request: tonic::Request<T>) -> (T, RequestParts) {
    let (metadata, extensions, request) = request.into_parts();
    let mut request_builder = Request::builder().method(Method::POST);
    *request_builder.headers_mut().unwrap() = metadata.into_headers();
    *request_builder.extensions_mut().unwrap() = extensions;
    let (parts, ()) = request_builder.body(()).unwrap().into_parts();
    (request, parts)
}

#[cfg(feature = "grpc")]
pin_project! {
    pub struct GrpcClientStream<O> {
        #[pin]
        stream: tonic::Streaming<O>,
    }
}

#[cfg(feature = "grpc")]
impl<O> Stream for GrpcClientStream<O> {
    type Item = Result<O, TwirpError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<O, TwirpError>>> {
        self.as_mut()
            .project()
            .stream
            .poll_next(cx)
            .map(|opt| opt.map(|r| Ok(r?)))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}

pub async fn twirp_error_from_response(response: impl IntoResponse) -> TwirpError {
    let (parts, body) = response.into_response().into_parts();
    let body = match body.collect().await {
        Ok(body) => body.to_bytes(),
        Err(e) => {
            error!(
                "Failed to load the body of the HTTP payload when building a TwirpError from a generic HTTP response: {e}"
            );
            return TwirpError::wrap(
                TwirpErrorCode::Internal,
                "Failed to map an internal error",
                e,
            );
        }
    };
    Response::from_parts(parts, body).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::twirp_fallback;
    #[cfg(feature = "grpc")]
    use axum::http::uri::PathAndQuery;
    use axum::http::{Method, Request, StatusCode};
    use http_body_util::BodyExt;
    use prost::Message;
    #[cfg(feature = "grpc")]
    use tonic::Code;
    #[cfg(feature = "grpc")]
    use tonic::client::Grpc;
    #[cfg(feature = "grpc")]
    use tonic_prost::ProstCodec;
    use tower_service::Service;

    const FILE_DESCRIPTOR_SET_BYTES: &[u8] = &[
        10, 107, 10, 21, 101, 120, 97, 109, 112, 108, 101, 95, 115, 101, 114, 118, 105, 99, 101,
        46, 112, 114, 111, 116, 111, 18, 7, 112, 97, 99, 107, 97, 103, 101, 34, 11, 10, 9, 77, 121,
        77, 101, 115, 115, 97, 103, 101, 74, 52, 10, 6, 18, 4, 0, 0, 5, 1, 10, 8, 10, 1, 12, 18, 3,
        0, 0, 18, 10, 8, 10, 1, 2, 18, 3, 2, 0, 16, 10, 10, 10, 2, 4, 0, 18, 4, 4, 0, 5, 1, 10, 10,
        10, 3, 4, 0, 1, 18, 3, 4, 8, 17, 98, 6, 112, 114, 111, 116, 111, 51,
    ];

    #[derive(Message, ReflectMessage, PartialEq)]
    #[prost_reflect(
        file_descriptor_set_bytes = "crate::codegen::tests::FILE_DESCRIPTOR_SET_BYTES",
        message_name = "package.MyMessage"
    )]
    pub struct MyMessage {}

    #[tokio::test]
    async fn test_bad_route() {
        let router = TwirpRouter::new(()).build().fallback(twirp_fallback);
        let response = router
            .into_service()
            .call(Request::new(Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            b"{\"code\":\"bad_route\",\"msg\":\"/ is not a supported Twirp method\"}".as_slice()
        );
    }

    #[tokio::test]
    async fn test_no_content_type() {
        let router = TwirpRouter::new(())
            .route(
                "/package.MyService/MyMethod",
                |(), request: MyMessage, _, _| async move { Ok(request) },
            )
            .build();
        let response = router
            .into_service()
            .call(
                Request::builder()
                    .method(Method::POST)
                    .uri("/package.MyService/MyMethod")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            b"{\"code\":\"malformed\",\"msg\":\"No content-type header\"}".as_slice()
        );
    }

    #[tokio::test]
    async fn test_ok_binary() {
        let router = TwirpRouter::new(())
            .route(
                "/package.MyService/MyMethod",
                |(), request: MyMessage, _, _| async move { Ok(request) },
            )
            .build();
        let response = router
            .into_service()
            .call(
                Request::builder()
                    .method(Method::POST)
                    .header(CONTENT_TYPE, APPLICATION_PROTOBUF)
                    .uri("/package.MyService/MyMethod")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            [].as_slice()
        );
    }

    #[tokio::test]
    async fn test_bad_binary() {
        let router = TwirpRouter::new(())
            .route(
                "/package.MyService/MyMethod",
                |(), request: MyMessage, _, _| async move { Ok(request) },
            )
            .build();
        let response = router
            .into_service()
            .call(
                Request::builder()
                    .method(Method::POST)
                    .header(CONTENT_TYPE, APPLICATION_PROTOBUF)
                    .uri("/package.MyService/MyMethod")
                    .body(Body::from(b"1234".to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            b"{\"code\":\"malformed\",\"msg\":\"Invalid binary protobuf request: failed to decode Protobuf message: buffer underflow\"}".as_slice()
        );
    }

    #[tokio::test]
    async fn test_ok_json() {
        let router = TwirpRouter::new(())
            .route(
                "/package.MyService/MyMethod",
                |(), request: MyMessage, _, _| async move { Ok(request) },
            )
            .build();
        let response = router
            .into_service()
            .call(
                Request::builder()
                    .method(Method::POST)
                    .header(CONTENT_TYPE, APPLICATION_JSON)
                    .uri("/package.MyService/MyMethod")
                    .body(Body::from(b"{}".to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            b"{}".as_slice()
        );
    }

    #[tokio::test]
    async fn test_bad_json() {
        let router = TwirpRouter::new(())
            .route(
                "/package.MyService/MyMethod",
                |(), request: MyMessage, _, _| async move { Ok(request) },
            )
            .build();
        let response = router
            .into_service()
            .call(
                Request::builder()
                    .method(Method::POST)
                    .header(CONTENT_TYPE, APPLICATION_JSON)
                    .uri("/package.MyService/MyMethod")
                    .body(Body::from(b"foo".to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            b"{\"code\":\"malformed\",\"msg\":\"Invalid JSON protobuf request: expected ident at line 1 column 2\"}".as_slice()
        );
    }

    #[tokio::test]
    async fn test_bad_content_type() {
        let router = TwirpRouter::new(())
            .route(
                "/package.MyService/MyMethod",
                |(), request: MyMessage, _, _| async move { Ok(request) },
            )
            .build();
        let response = router
            .into_service()
            .call(
                Request::builder()
                    .method(Method::POST)
                    .header(CONTENT_TYPE, "foo/bar")
                    .uri("/package.MyService/MyMethod")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            b"{\"code\":\"malformed\",\"msg\":\"Unsupported content type: foo/bar\"}".as_slice()
        );
    }

    #[cfg(feature = "grpc")]
    #[tokio::test]
    async fn test_grpc_request() {
        let router = GrpcRouter::new(())
            .route(
                "/package.MyService/MyMethod",
                |(), request: MyMessage, _| async move { Ok(request) },
            )
            .build();
        let path = PathAndQuery::from_static("/package.MyService/MyMethod");
        let response: MyMessage = Grpc::new(router)
            .unary(
                tonic::Request::new(MyMessage {}),
                path,
                ProstCodec::default(),
            )
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response, MyMessage {})
    }

    #[cfg(feature = "grpc")]
    #[tokio::test]
    async fn test_grpc_request_with_error() {
        let router = GrpcRouter::new(())
            .route(
                "/package.MyService/MyMethod",
                |(), _: MyMessage, _| async move {
                    Err::<MyMessage, _>(TwirpError::not_found("foo not found"))
                },
            )
            .build();
        let path = PathAndQuery::from_static("/package.MyService/MyMethod");
        let status = Grpc::new(router)
            .unary::<_, MyMessage, _>(
                tonic::Request::new(MyMessage {}),
                path,
                ProstCodec::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::NotFound);
        assert_eq!(status.message(), "foo not found");
    }
}
