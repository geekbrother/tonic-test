use super::Decoder;
use crate::body::BoxBody;
use crate::metadata::MetadataMap;
use crate::{Code, Status};
use bytes::{Buf, BufMut, Bytes, BytesMut, IntoBuf};
use futures_core::Stream;
use futures_util::{future, ready};
use http::StatusCode;
use http_body::Body;
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::{debug, trace};

// #[derive(Debug)]
pub struct Streaming<T> {
    decoder: Box<dyn Decoder<Item = T, Error = Status> + Send + 'static>,
    body: BoxBody,
    state: State,
    direction: Direction,
    buf: BytesMut,
}

impl<T> Unpin for Streaming<T> {}

#[derive(Debug)]
enum State {
    ReadHeader,
    ReadBody { compression: bool, len: usize },
}

#[derive(Debug)]
enum Direction {
    Request,
    Response(StatusCode),
    EmptyResponse,
}

impl<T> Streaming<T> {
    pub fn new_response<B, D>(decoder: D, body: B, status_code: StatusCode) -> Self
    where
        B: Body + Send + 'static,
        B::Data: Into<Bytes>,
        B::Error: Into<crate::Error>,
        D: Decoder<Item = T, Error = Status> + Send + 'static,
    {
        Self {
            decoder: Box::new(decoder),
            body: BoxBody::map_from(body),
            state: State::ReadHeader,
            direction: Direction::Response(status_code),
            // FIXME: update this with a reasonable size
            buf: BytesMut::with_capacity(1024 * 1024),
        }
    }

    pub fn new_empty<B, D>(decoder: D, body: B) -> Self
    where
        B: Body + Send + 'static,
        B::Data: Into<Bytes>,
        B::Error: Into<crate::Error>,
        D: Decoder<Item = T, Error = Status> + Send + 'static,
    {
        Self {
            decoder: Box::new(decoder),
            body: BoxBody::map_from(body),
            state: State::ReadHeader,
            direction: Direction::EmptyResponse,
            // FIXME: update this with a reasonable size
            buf: BytesMut::with_capacity(1024 * 1024),
        }
    }

    pub fn new_request<B, D>(decoder: D, body: B) -> Self
    where
        B: Body + Send + 'static,
        B::Data: Into<Bytes>,
        B::Error: Into<crate::Error>,
        D: Decoder<Item = T, Error = Status> + Send + 'static,
    {
        Self {
            decoder: Box::new(decoder),
            body: BoxBody::map_from(body),
            state: State::ReadHeader,
            direction: Direction::Request,
            // FIXME: update this with a reasonable size
            buf: BytesMut::with_capacity(1024 * 1024),
        }
    }
}

impl<T> Streaming<T> {
    // pub async fn message(&mut self) -> Option<Result<T::Item, Status>> {
    //     future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    // }

    pub async fn trailers(&mut self) -> Result<Option<MetadataMap>, Status> {
        let map =
            future::poll_fn(|cx| unsafe { Pin::new_unchecked(&mut self.body) }.poll_trailers(cx))
                .await
                .map_err(|e| Status::from_error(&e))?;
        Ok(map.map(MetadataMap::from_headers))
    }

    fn decode_chunk(&mut self) -> Result<Option<T>, Status> {
        let mut buf = (&self.buf[..]).into_buf();

        if let State::ReadHeader = self.state {
            if buf.remaining() < 5 {
                return Ok(None);
            }

            let is_compressed = match buf.get_u8() {
                0 => false,
                1 => {
                    trace!("message compressed, compression not supported yet");
                    return Err(Status::new(
                        Code::Unimplemented,
                        "Message compressed, compression not supported yet.".to_string(),
                    ));
                }
                f => {
                    trace!("unexpected compression flag");
                    return Err(Status::new(
                        Code::Internal,
                        format!("Unexpected compression flag: {}", f),
                    ));
                }
            };
            let len = buf.get_u32_be() as usize;

            self.state = State::ReadBody {
                compression: is_compressed,
                len,
            }
        }

        if let State::ReadBody { len, .. } = &self.state {
            if buf.remaining() < *len {
                return Ok(None);
            }

            // advance past the header
            self.buf.advance(5);

            match self.decoder.decode(&mut self.buf) {
                Ok(Some(msg)) => {
                    self.state = State::ReadHeader;
                    return Ok(Some(msg));
                }
                Ok(None) => return Ok(None),
                Err(e) => {
                    return Err(e);
                }
            }
        }

        Ok(None)
    }
}

impl<T> Stream for Streaming<T> {
    type Item = Result<T, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // TODO: implement the ability to poll trailers when we _know_ that
            // the comnsumer of this stream will only poll for the first message.
            // This means we skip the poll_trailers step.
            match self.decode_chunk()? {
                Some(item) => return Poll::Ready(Some(Ok(item))),
                None => (),
            }

            // FIXME: Figure out how to verify that this is safe
            let chunk = match ready!(unsafe { Pin::new_unchecked(&mut self.body) }.poll_data(cx)) {
                Some(Ok(d)) => Some(d),
                Some(Err(e)) => {
                    let err: crate::Error = e.into();
                    debug!("decoder inner stream error: {:?}", err);
                    let status = Status::from_error(&*err);
                    Err(status)?;
                    break;
                }
                None => None,
            };

            if let Some(data) = chunk {
                self.buf.put(data);
            } else {
                // FIXME: get BytesMut to impl `Buf` directlty?
                let buf1 = (&self.buf[..]).into_buf();
                if buf1.has_remaining() {
                    trace!("unexpected EOF decoding stream");
                    Err(Status::new(
                        Code::Internal,
                        "Unexpected EOF decoding stream.".to_string(),
                    ))?;
                } else {
                    break;
                }
            }
        }

        if let Direction::Response(status) = self.direction {
            match ready!(unsafe { Pin::new_unchecked(&mut self.body) }.poll_trailers(cx)) {
                Ok(trailer) => {
                    if let Err(e) = crate::status::infer_grpc_status(trailer, status) {
                        return Some(Err(e)).into();
                    }
                }
                Err(e) => {
                    let err: crate::Error = e.into();
                    debug!("decoder inner trailers error: {:?}", err);
                    let status = Status::from_error(&*err);
                    return Some(Err(status)).into();
                }
            }
        }

        Poll::Ready(None)
    }
}

impl<T> fmt::Debug for Streaming<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Streaming")
    }
}