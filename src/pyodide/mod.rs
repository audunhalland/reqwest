#![allow(missing_docs)]
#![allow(unused)]

use core::str;
use std::{fmt::Debug, future::Future};

use bytes::{Bytes, BytesMut};
use futures_core::{stream::BoxStream, Stream};
use futures_util::{StreamExt, TryStreamExt};
use http::{HeaderMap, Method, StatusCode};
use pyo3::{
    prelude::*,
    types::{PyBytes, PyDict},
};
use sync_wrapper::SyncStream;
use url::Url;

use crate::{
    error::{BoxError, Kind},
    IntoUrl,
};

#[cfg(feature = "json")]
use serde::{de::DeserializeOwned, ser::Serialize};

#[derive(Debug)]
pub struct ClientBuilder;

impl ClientBuilder {
    pub fn build(self) -> crate::Result<Client> {
        Ok(Client {})
    }

    pub fn danger_accept_invalid_certs(mut self, accept_invalid_certs: bool) -> Self {
        self
    }

    pub fn http2_prior_knowledge(mut self) -> Self {
        self
    }
}

#[derive(Clone, Debug)]
pub struct Client {}

impl Client {
    pub fn new() -> Self {
        Self {}
    }

    pub fn builder() -> ClientBuilder {
        ClientBuilder
    }

    pub fn get<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    pub fn post<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    pub fn put<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PUT, url)
    }

    pub fn patch<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PATCH, url)
    }

    pub fn delete<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::DELETE, url)
    }

    pub fn head<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::HEAD, url)
    }

    pub fn request<U: IntoUrl>(&self, method: Method, url: U) -> RequestBuilder {
        let req = url.into_url().map(move |url| Request::new(method, url));
        RequestBuilder {
            client: self.clone(),
            request: req,
        }
    }

    fn execute_request(&self, req: Request) -> impl Future<Output = crate::Result<Response>> {
        // self.merge_headers(&mut req);
        pyodide_fetch(req)
    }
}

#[derive(Debug)]
pub struct RequestBuilder {
    client: Client,
    request: crate::Result<Request>,
}

impl RequestBuilder {
    pub fn headers(mut self, headers: crate::header::HeaderMap) -> RequestBuilder {
        if let Ok(ref mut req) = self.request {
            crate::util::replace_headers(&mut req.headers, headers);
        }
        self
    }

    #[cfg(feature = "json")]
    /// Set the request json
    pub fn json<T: Serialize + ?Sized>(mut self, json: &T) -> RequestBuilder {
        use http::{header::CONTENT_TYPE, HeaderValue};

        let mut error = None;
        if let Ok(ref mut req) = self.request {
            match serde_json::to_vec(json) {
                Ok(body) => {
                    req.headers
                        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

                    req.body = Some(Body::new(single_chunk(Ok(Bytes::from_iter(body))).boxed()));
                }
                Err(err) => error = Some(crate::error::builder(err)),
            }
        }
        if let Some(err) = error {
            self.request = Err(err);
        }
        self
    }

    pub fn body<T: Into<Body>>(mut self, body: T) -> RequestBuilder {
        if let Ok(ref mut req) = self.request {
            req.body = Some(body.into());
        }
        self
    }

    pub async fn send(self) -> crate::Result<Response> {
        let req = self.request?;
        self.client.execute_request(req).await
    }
}

fn single_chunk(
    chunk: crate::Result<Bytes>,
) -> impl Stream<Item = crate::Result<Bytes>> + Send + 'static {
    futures_util::stream::iter([chunk])
}

#[derive(Debug)]
pub struct Request {
    method: Method,
    url: Url,
    headers: HeaderMap,
    body: Option<Body>,
    cors: bool,
    // pub(super) credentials: Option<RequestCredentials>,
}

impl Request {
    fn new(method: Method, url: Url) -> Self {
        Self {
            method,
            url,
            headers: Default::default(),
            body: None,
            cors: false,
        }
    }
}

#[derive(Debug)]
pub struct Response {
    http: http::Response<Body>,
    url: Box<Url>,
}

impl Response {
    pub fn status(&self) -> StatusCode {
        self.http.status()
    }

    pub fn headers(&self) -> &HeaderMap {
        self.http.headers()
    }

    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        self.http.headers_mut()
    }

    pub fn content_length(&self) -> Option<u64> {
        self.headers()
            .get(http::header::CONTENT_LENGTH)?
            .to_str()
            .ok()?
            .parse()
            .ok()
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    #[cfg(feature = "json")]
    pub async fn json<T: DeserializeOwned>(self) -> crate::Result<T> {
        let full = self.http.into_body().bytes().await?;
        serde_json::from_slice(&full).map_err(crate::error::decode)
    }

    pub async fn text(self) -> crate::Result<String> {
        let full = self.http.into_body().bytes().await?;
        match str::from_utf8(&full) {
            Ok(str) => Ok(str.to_string()),
            Err(err) => Err(crate::Error::new(Kind::Decode, Some("not utf8-encoded"))),
        }
    }

    pub async fn bytes(self) -> crate::Result<Bytes> {
        self.http.into_body().bytes().await
    }

    pub fn bytes_stream(self) -> impl futures_core::Stream<Item = crate::Result<Bytes>> {
        self.http.into_body().stream
    }

    // util methods

    /// Turn a response into an error if the server returned an error.
    pub fn error_for_status(self) -> crate::Result<Self> {
        let status = self.status();
        if status.is_client_error() || status.is_server_error() {
            Err(crate::error::status_code(*self.url, status))
        } else {
            Ok(self)
        }
    }

    /// Turn a reference to a response into an error if the server returned an error.
    pub fn error_for_status_ref(&self) -> crate::Result<&Self> {
        let status = self.status();
        if status.is_client_error() || status.is_server_error() {
            Err(crate::error::status_code(*self.url.clone(), status))
        } else {
            Ok(self)
        }
    }
}

pub struct Body {
    stream: SyncStream<BoxStream<'static, crate::Result<Bytes>>>,
}

impl Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Body").finish()
    }
}

impl Body {
    fn new(stream: BoxStream<'static, crate::Result<Bytes>>) -> Self {
        Self {
            stream: SyncStream::new(stream),
        }
    }

    async fn bytes(self) -> crate::Result<Bytes> {
        let concatenated: BytesMut = self.stream.try_collect().await?;
        Ok(concatenated.freeze())
    }
}

impl From<Bytes> for Body {
    fn from(bytes: Bytes) -> Body {
        Self {
            stream: SyncStream::new(single_chunk(Ok(bytes)).boxed()),
        }
    }
}

impl From<Vec<u8>> for Body {
    fn from(vec: Vec<u8>) -> Body {
        Body {
            stream: SyncStream::new(single_chunk(Ok(Bytes::copy_from_slice(&vec))).boxed()),
        }
    }
}

async fn pyodide_fetch(req: Request) -> crate::Result<Response> {
    let pyfetch = Python::with_gil(move |py| call_pyfetch(req, py))?;

    let py_fetch_response = pyfetch.await.map_err(py_err)?;

    Python::with_gil(move |py| to_response(py_fetch_response, py))
}

fn call_pyfetch(
    req: Request,
    py: Python,
) -> crate::Result<impl Future<Output = PyResult<PyObject>>> {
    let pyodide = PyModule::import_bound(py, "pyodide").map_err(py_err)?;
    let http = pyodide.getattr("http").map_err(py_err)?;
    let pyfetch = http.getattr("pyfetch").map_err(py_err)?;

    let url = req.url.to_string();

    let mut py_headers = PyDict::new_bound(py);
    for (name, value) in req.headers {
        let Some(name) = name else {
            continue;
        };

        py_headers.set_item(name.to_string(), value.to_str().map_err(py_err)?);
    }

    let mut kwargs = PyDict::new_bound(py);
    kwargs.set_item("method", req.method.to_string());
    kwargs.set_item("headers", py_headers);

    if let Some(body) = req.body {
        todo!("set body");
        // kwargs.set_item("body", todo!("make body"));
    }

    let coroutine = pyfetch.call((url,), Some(&kwargs)).map_err(py_err)?;

    pyo3_async_runtimes::tokio::into_future(coroutine).map_err(py_err)
}

fn to_response(py_fetch_response: Py<PyAny>, py: Python) -> crate::Result<Response> {
    let (builder, url, body) = map_response(py_fetch_response, py).map_err(py_err)?;
    let url: Url = url
        .parse()
        .map_err(|_| crate::Error::new(Kind::Decode, Some("url parse")))?;
    let http_response = builder
        .body(body)
        .map_err(|_| crate::Error::new(Kind::Body, Some("could not set body")))?;

    Ok(Response {
        http: http_response,
        url: Box::new(url),
    })
}

fn map_response(
    py_fetch_response: Py<PyAny>,
    py: Python,
) -> PyResult<(http::response::Builder, String, Body)> {
    let js_response = py_fetch_response.getattr(py, "js_response")?;

    let status: u16 = js_response.getattr(py, "status")?.extract(py)?;
    let url: String = js_response.getattr(py, "url")?.extract(py)?;
    let headers: Bound<PyDict> = js_response.getattr(py, "headers")?.extract(py)?;
    let body_reader = js_response
        .getattr(py, "body")?
        .call_method1(py, "getReader", ())?;

    let mut builder = http::Response::builder().status(status);

    for (key, value) in headers {
        let key: String = key.extract()?;
        let value: String = value.extract()?;

        builder = builder.header(key, value);
    }

    Ok((
        builder,
        url,
        Body {
            stream: SyncStream::new(py_reader_stream::bytes_stream(body_reader).boxed()),
        },
    ))
}

fn py_err(err: impl Into<BoxError>) -> crate::Error {
    crate::Error::new(Kind::Request, Some(err))
}

mod py_reader_stream {
    use bytes::Bytes;
    use futures_core::Stream;
    use pyo3::{
        prelude::*,
        types::{PyBytes, PyDict},
    };

    use crate::error::Kind;

    use super::py_err;

    pub fn bytes_stream(py_reader: Py<PyAny>) -> impl Stream<Item = crate::Result<Bytes>> {
        futures_util::stream::try_unfold(py_reader, |py_reader| async {
            if let Some(chunk) = read_next_chunk(&py_reader).await? {
                Ok(Some((chunk, py_reader)))
            } else {
                Ok(None)
            }
        })
    }

    async fn read_next_chunk(py_reader: &Py<PyAny>) -> crate::Result<Option<Bytes>> {
        let future = Python::with_gil(|py| {
            let coroutine = py_reader
                .call_method1(py, "read", ())
                .map_err(py_err)?
                .into_bound(py);
            pyo3_async_runtimes::tokio::into_future(coroutine).map_err(py_err)
        })?;

        let frame = future
            .await
            .map_err(|_| crate::Error::new(Kind::Decode, Some("body network error")))?;

        Python::with_gil(|py| {
            let frame = frame.into_bound(py);
            let frame_value = frame.getattr("value").map_err(py_err)?;

            if frame_value.is_none() {
                Ok(None)
            } else {
                let py_bytes: Bound<'_, PyBytes> = frame_value
                    .call_method1("to_bytes", ())
                    .map_err(py_err)?
                    .extract()
                    .map_err(py_err)?;

                Ok(Some(Bytes::copy_from_slice(py_bytes.as_bytes())))
            }
        })
    }
}
