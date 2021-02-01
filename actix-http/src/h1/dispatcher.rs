use std::{
    cell::RefCell,
    fmt,
    future::Future,
    io,
    marker::PhantomData,
    net,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed, FramedParts};
use actix_rt::time::{sleep_until, Instant, Sleep};
use actix_service::Service;
use bytes::{Buf, BytesMut};
use futures_core::ready;
use log::{error, trace};
use pin_project_lite::pin_project;

use crate::body::{BodySize, MessageBody, ResponseBody};
use crate::config::ServiceConfig;
use crate::error::{DispatchError, Error};
use crate::error::{ParseError, PayloadError};
use crate::request::Request;
use crate::response::Response;
use crate::service::HttpFlow;
use crate::OnConnectData;

use super::codec::Codec;
use super::decoder::{PayloadDecoder, PayloadItem, PayloadType, MAX_BUFFER_SIZE};
use super::payload::{Payload, PayloadSender, PayloadStatus};
use super::Message;

const LW_BUFFER_SIZE: usize = 1024;
const HW_BUFFER_SIZE: usize = 1024 * 8;

pub(crate) async fn h1_dispatch<Io, S, B, X, U>(
    io: Io,
    flow: Rc<RefCell<HttpFlow<S, X, U>>>,
    on_connect_data: OnConnectData,
    peer_addr: Option<net::SocketAddr>,
    config: ServiceConfig,
) -> Result<(), DispatchError>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request>,
    S::Error: Into<Error>,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request, Response = Request>,
    X::Error: Into<Error>,
    U: Service<(Request, Framed<Io, Codec>), Response = ()>,
    U::Error: fmt::Display,
{
    // initial client timeout
    let sleep = actix_rt::time::sleep(std::time::Duration::from_secs(5));
    // pin sleep on stack so no pin_project is needed for dispatcher.(for better code hinting).
    futures_util::pin_mut!(sleep);

    // construct dispatcher for request handling.
    let mut dispatcher =
        Dispatcher::new(io, flow, on_connect_data, peer_addr, config, sleep);

    loop {
        // read from io and fill read buffer.
        // state indicate if all data has been read.
        let state = match dispatcher.read().await {
            Ok(state) => state,
            Err(e) => {
                error!("DispatchReader error: {}", e);
                break;
            }
        };

        // decode read buffer and call service future.
        // return response body.
        let payload = match dispatcher.codec.decode(&mut dispatcher.read_buf) {
            // decode successful.
            Ok(Some((mut req, pl))) => {
                trace!(
                    "http message received: {:?}\r\n\r\npayload type: {:?}",
                    req,
                    pl
                );

                // write dispatcher's state to request.
                dispatcher.set_peer_addr(&mut req);
                dispatcher.set_on_connect_data(&mut req);

                // payload type indicate how request is handled.
                // return decoder and payload sender in tuple.
                let mut payload = match pl {
                    // upgrade the dispatcher and poll on it exclusively.
                    PayloadType::Stream(_) if dispatcher.is_upgradeable() => {
                        return dispatcher.upgrade(req).await
                    }
                    // generate and set payload for dispatcher.
                    PayloadType::Stream(decoder) | PayloadType::Payload(decoder) => {
                        // Payload is a smart pointer passed to service call where incoming
                        // io data can be read from.
                        // PayloadSender is the sink of Payload for pushing new data into it.
                        let (ps, pl) = Payload::create(false);
                        let (req1, _) = req.replace_payload(crate::Payload::H1(pl));
                        req = req1;
                        Some((decoder, ps))
                    }
                    // no payload so ignore.
                    PayloadType::None => None,
                };

                // Handle `EXPECT: 100-Continue` header.
                // expect should be polled exclusively until finished.
                if req.head().expect() {
                    req = dispatcher.expect(req).await?;
                }

                // get response and body from service_call.
                let (res, body) = dispatcher.service_call(req, payload.as_mut()).await?;

                // encode response and write it to buffer first.
                // body would be handled later in send_payload
                if let Err(e) = dispatcher
                    .codec
                    .encode(Message::Item((res, body.size())), &mut dispatcher.write_buf)
                {
                    if let Some((_, payload)) = payload {
                        payload.set_error(PayloadError::Incomplete(None));
                    }
                    return Err(DispatchError::Io(e));
                }

                match body.size() {
                    BodySize::None | BodySize::Empty => None,
                    _ => Some(body),
                }
            }
            // decode failure because not enough data read.
            Ok(None) => {
                match state {
                    // still can read more. continue.
                    DispatchReaderState::Partial => continue,
                    // nothing can be read anymore. stop dispatcher.
                    DispatchReaderState::All => break,
                }
            }
            Err(e) => {
                error!("DispatchReader error: {:?}", e);
                break;
            }
        };

        // send payload to client.
        dispatcher.send_payload(payload).await?;
    }

    dispatcher.stop().await
}

pub struct Dispatcher<'a, Io, S, B, X, U>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request>,
    S::Error: Into<Error>,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request, Response = Request>,
    X::Error: Into<Error>,
    U: Service<(Request, Framed<Io, Codec>), Response = ()>,
    U::Error: fmt::Display,
{
    io: Io,
    flow: Rc<RefCell<HttpFlow<S, X, U>>>,
    on_connect_data: OnConnectData,
    peer_addr: Option<net::SocketAddr>,
    codec: Codec,
    read_buf: BytesMut,
    write_buf: BytesMut,
    sleep: Pin<&'a mut Sleep>,
    _body: PhantomData<B>,
}

impl<'a, Io, S, B, X, U> Dispatcher<'a, Io, S, B, X, U>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request>,
    S::Error: Into<Error>,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request, Response = Request>,
    X::Error: Into<Error>,
    U: Service<(Request, Framed<Io, Codec>), Response = ()>,
    U::Error: fmt::Display,
{
    fn new(
        io: Io,
        flow: Rc<RefCell<HttpFlow<S, X, U>>>,
        on_connect_data: OnConnectData,
        peer_addr: Option<net::SocketAddr>,
        config: ServiceConfig,
        sleep: Pin<&'a mut Sleep>,
    ) -> Self {
        Self {
            io,
            flow,
            on_connect_data,
            peer_addr,
            codec: Codec::new(config),
            read_buf: BytesMut::with_capacity(HW_BUFFER_SIZE),
            write_buf: BytesMut::with_capacity(HW_BUFFER_SIZE),
            sleep,
            _body: PhantomData,
        }
    }

    // merge on_connect_ext data into request extensions
    #[inline(always)]
    fn set_on_connect_data(&mut self, req: &mut Request) {
        self.on_connect_data.merge_into(req);
    }

    #[inline(always)]
    fn set_peer_addr(&mut self, req: &mut Request) {
        req.head_mut().peer_addr = self.peer_addr;
    }

    #[inline(always)]
    fn is_upgradeable(&self) -> bool {
        self.flow.borrow().upgrade.is_some()
    }

    // async reader for stream io.
    #[inline(always)]
    fn read(&mut self) -> DispatchReader<'_, Io> {
        DispatchReader::new(&mut self.io, &mut self.read_buf)
    }

    fn stop(self) -> DispatchShutDown<Io> {
        DispatchShutDown { io: self.io }
    }

    #[inline(always)]
    fn send_payload(
        &mut self,
        payload: Option<ResponseBody<B>>,
    ) -> DispatchWriter<'_, Io, B> {
        DispatchWriter {
            io: &mut self.io,
            codec: &mut self.codec,
            write_buf: &mut self.write_buf,
            payload,
        }
    }

    // call service future and resolve it.
    async fn service_call<'s: 'p, 'p>(
        &'s mut self,
        req: Request,
        payload: Option<&'p mut (PayloadDecoder, PayloadSender)>,
    ) -> Result<(Response<()>, ResponseBody<B>), DispatchError> {
        // manually construct reader so self can be borrow with individual fields.
        let reader = DispatchReader::new(&mut self.io, &mut self.read_buf);

        // this is a wrapper to make sure borrowing dispatcher state is possible in
        // child async blocks.
        let flow = &self.flow;
        let fut = async move {
            let fut = flow.borrow_mut().service.call(req);
            fut.await
        };

        // poll service future read more from stream concurrently.
        let res = DispatchServiceHandler {
            fut,
            payload,
            reader,
        }
        .await;

        // write response and handle error.
        match res {
            Ok(res) => Ok(res.into().replace_body(())),
            Err(e) => {
                let res: Response = e.into().into();
                let (res, body) = res.replace_body(());
                Ok((res, body.into_body()))
            }
        }
    }

    // handle upgrade request. After upgrade dispatcher is mostly consumed.
    // dispatcher future would be polled on this future exclusively until resolved.
    async fn upgrade(self, req: Request) -> Result<(), DispatchError> {
        trace!("initiate upgrade handling");
        let mut parts = FramedParts::with_read_buf(self.io, self.codec, self.read_buf);
        parts.write_buf = self.write_buf;
        let framed = Framed::from_parts(parts);
        let fut = self
            .flow
            .borrow_mut()
            .upgrade
            .as_mut()
            .unwrap()
            .call((req, framed));

        fut.await.map_err(|e| {
            error!("Upgrade handler error: {}", e);
            DispatchError::Upgrade
        })
    }

    // handle expect request.
    async fn expect(&mut self, req: Request) -> Result<Request, Error> {
        trace!("initiate expect handling");
        let fut = self.flow.borrow_mut().expect.call(req);
        let res = fut.await.map_err(Into::into)?;
        // on success write continue to buffer.
        self.write_buf
            .extend_from_slice(b"HTTP/1.1 100 Continue\r\n\r\n");
        Ok(res)
    }
}

// service handler for polling service future and read more from stream concurrently
pin_project! {
    struct DispatchServiceHandler<'r, 'p, Io, Fut> {
        #[pin]
        fut: Fut,
        payload: Option<&'p mut (PayloadDecoder, PayloadSender)>,
        reader: DispatchReader<'r, Io>
    }
}

impl<Io, Fut> Future for DispatchServiceHandler<'_, '_, Io, Fut>
where
    Fut: Future,
    Io: AsyncRead + Unpin,
{
    type Output = Fut::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        match this.fut.poll(cx) {
            Poll::Ready(res) => Poll::Ready(res),
            Poll::Pending => {
                // service call is pending. it may want to read more data from io.
                if let Some((decoder, payload)) = this.payload {
                    match payload.need_read(cx) {
                        PayloadStatus::Read => {
                            let mut read = false;
                            loop {
                                match decoder.decode(&mut *this.reader.read_buf) {
                                    Ok(Some(PayloadItem::Chunk(chunk))) => {
                                        payload.feed_data(chunk);
                                        read = true;
                                    }
                                    Ok(Some(PayloadItem::Eof)) => {
                                        payload.feed_eof();
                                        *this.payload = None;
                                        break;
                                    }
                                    Ok(None) => {
                                        match ready!(Pin::new(&mut *this.reader).poll(cx))
                                        {
                                            Ok(_todo) => {}
                                            Err(_) => {
                                                payload.set_error(
                                                    PayloadError::EncodingCorrupted,
                                                );
                                                *this.payload = None;
                                                break;
                                            }
                                        }
                                    }

                                    Err(_) => {
                                        payload
                                            .set_error(PayloadError::EncodingCorrupted);
                                        *this.payload = None;
                                        break;
                                    }
                                }
                            }

                            if read {
                                self.poll(cx)
                            } else {
                                Poll::Pending
                            }
                        }
                        PayloadStatus::Pause => Poll::Pending,
                        PayloadStatus::Dropped => {
                            *this.payload = None;
                            Poll::Pending
                        }
                    }
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

trait ReadWriteBuf {
    // grow buffer according to HW_BUFFER_SIZE
    fn grow(&mut self);
}

impl ReadWriteBuf for BytesMut {
    #[inline(always)]
    fn grow(&mut self) {
        let remaining = self.capacity() - self.len();
        if remaining < LW_BUFFER_SIZE {
            self.reserve(HW_BUFFER_SIZE - remaining);
        }
    }
}

// reader handle reading from io to buffer.
struct DispatchReader<'a, Io> {
    io: &'a mut Io,
    read_some: bool,
    read_buf: &'a mut BytesMut,
}

impl<'a, Io> DispatchReader<'a, Io> {
    #[inline(always)]
    fn new(io: &'a mut Io, read_buf: &'a mut BytesMut) -> Self {
        Self {
            io,
            read_some: false,
            read_buf,
        }
    }
}

// notify if all data has been read from io
enum DispatchReaderState {
    Partial,
    All,
}

impl<Io> Future for DispatchReader<'_, Io>
where
    Io: AsyncRead + Unpin,
{
    type Output = Result<DispatchReaderState, io::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        let mut io = Pin::new(&mut *this.io);
        loop {
            // grow buffer according to HW_BUFFER_SIZE
            this.read_buf.grow();

            match actix_codec::poll_read_buf(io.as_mut(), cx, this.read_buf)? {
                // read done. exit.
                Poll::Ready(0) => return Poll::Ready(Ok(DispatchReaderState::All)),
                Poll::Ready(_) => {
                    // buffer overflow. return error
                    if this.read_buf.len() > MAX_BUFFER_SIZE {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "buffer over flow",
                        )));
                    }
                    this.read_some = true;
                }
                Poll::Pending => {
                    return if this.read_some {
                        Poll::Ready(Ok(DispatchReaderState::Partial))
                    } else {
                        Poll::Pending
                    }
                }
            }
        }
    }
}

pin_project! {
    struct DispatchWriter<'a, Io, B> {
        io: &'a mut Io,
        codec: &'a mut Codec,
        write_buf: &'a mut BytesMut,
        #[pin]
        payload: Option<ResponseBody<B>>
    }
}

impl<Io, B> Future for DispatchWriter<'_, Io, B>
where
    Io: AsyncWrite + Unpin,
    B: MessageBody,
{
    type Output = Result<(), DispatchError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        let mut io = Pin::new(this.io);

        if let Some(mut payload) = this.payload.as_mut().as_pin_mut() {
            // try to write payload stream to write buffer.
            while this.write_buf.len() < HW_BUFFER_SIZE {
                this.write_buf.grow();
                match payload.as_mut().poll_next(cx) {
                    Poll::Ready(Some(Ok(item))) => {
                        this.codec
                            .encode(Message::Chunk(Some(item)), this.write_buf)?;
                    }
                    Poll::Ready(None) => {
                        this.codec.encode(Message::Chunk(None), this.write_buf)?;
                        // all buffer is written. remove payload so we don't poll
                        // payload stream any more.
                        this.payload.set(None);
                        break;
                    }
                    Poll::Ready(Some(Err(e))) => {
                        return Poll::Ready(Err(DispatchError::Service(e)))
                    }
                    Poll::Pending => break,
                }
            }
        }

        let len = this.write_buf.len();

        // write to socket if something is already in buffer.
        let mut written = 0;
        while written < len {
            match io.as_mut().poll_write(cx, &this.write_buf[written..]) {
                Poll::Pending => break,
                Poll::Ready(Ok(n)) => {
                    if n == 0 {
                        trace!("Disconnected during flush, written {}", written);
                        return Poll::Ready(Err(DispatchError::Io(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write frame to transport",
                        ))));
                    } else {
                        written += n
                    }
                }
                Poll::Ready(Err(e)) => {
                    trace!("Error during flush: {}", e);
                    return Poll::Ready(Err(DispatchError::Io(e)));
                }
            }
        }

        log::trace!("flushed {} bytes", written);

        // remove written data
        if written == len {
            // flushed same amount as in buffer, we dont need to reallocate
            unsafe {
                this.write_buf.set_len(0);
            }
        } else {
            this.write_buf.advance(written);
        }

        if this.payload.is_none() && this.write_buf.is_empty() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

struct DispatchShutDown<Io> {
    io: Io,
}

impl<Io> Future for DispatchShutDown<Io>
where
    Io: AsyncWrite + Unpin,
{
    type Output = Result<(), DispatchError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        match ready!(Pin::new(&mut this.io).poll_shutdown(cx)) {
            Ok(_) => Poll::Ready(Ok(())),
            Err(_) => {
                trace!("write task is closed with err during shutdown");
                Poll::Ready(Ok(()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str;

    use actix_service::fn_service;
    use futures_util::future::{lazy, ready};

    use super::*;
    use crate::test::TestBuffer;
    use crate::{error::Error, KeepAlive};
    use crate::{
        h1::{ExpectHandler, UpgradeHandler},
        test::TestSeqBuffer,
    };

    fn find_slice(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
        haystack[from..]
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn stabilize_date_header(payload: &mut [u8]) {
        let mut from = 0;

        while let Some(pos) = find_slice(&payload, b"date", from) {
            payload[(from + pos)..(from + pos + 35)]
                .copy_from_slice(b"date: Thu, 01 Jan 1970 12:34:56 UTC");
            from += 35;
        }
    }

    fn ok_service() -> impl Service<Request, Response = Response, Error = Error> {
        fn_service(|_req: Request| ready(Ok::<_, Error>(Response::Ok().finish())))
    }

    fn echo_path_service() -> impl Service<Request, Response = Response, Error = Error> {
        fn_service(|req: Request| {
            let path = req.path().as_bytes();
            ready(Ok::<_, Error>(Response::Ok().body(Body::from_slice(path))))
        })
    }

    fn echo_payload_service() -> impl Service<Request, Response = Response, Error = Error>
    {
        fn_service(|mut req: Request| {
            Box::pin(async move {
                use futures_util::stream::StreamExt as _;

                let mut pl = req.take_payload();
                let mut body = BytesMut::new();
                while let Some(chunk) = pl.next().await {
                    body.extend_from_slice(chunk.unwrap().chunk())
                }

                Ok::<_, Error>(Response::Ok().body(body))
            })
        })
    }

    #[actix_rt::test]
    async fn test_req_parse_err() {
        lazy(|cx| {
            let buf = TestBuffer::new("GET /test HTTP/1\r\n\r\n");

            let sleep = actix_rt::time::sleep(std::time::Duration::from_secs(5));
            futures_util::pin_mut!(sleep);

            let flow = HttpFlow::new(ok_service(), ExpectHandler, None);

            let mut h1 = Dispatcher::<_, _, _, _, UpgradeHandler>::new(
                buf,
                flow,
                OnConnectData::default(),
                None,
                ServiceConfig::default(),
                sleep,
            );

            match Pin::new(&mut h1).poll(cx) {
                Poll::Pending => panic!(),
                Poll::Ready(res) => assert!(res.is_err()),
            }

            assert_eq!(h1.io.write_buf[..26], b"HTTP/1.1 400 Bad Request\r\n");
        })
        .await;
    }

    #[actix_rt::test]
    async fn test_expect() {
        lazy(|cx| {
            let mut buf = TestSeqBuffer::empty();
            let cfg = ServiceConfig::new(KeepAlive::Disabled, 0, 0, false, None);

            let services = HttpFlow::new(echo_payload_service(), ExpectHandler, None);

            let h1 = Dispatcher::<_, _, _, _, UpgradeHandler>::new(
                buf.clone(),
                cfg,
                services,
                OnConnectData::default(),
                None,
            );

            buf.extend_read_buf(
                "\
                POST /upload HTTP/1.1\r\n\
                Content-Length: 5\r\n\
                Expect: 100-continue\r\n\
                \r\n\
                ",
            );

            futures_util::pin_mut!(h1);

            assert!(h1.as_mut().poll(cx).is_pending());
            assert!(matches!(&h1.inner, DispatcherState::Normal(_)));

            // polls: manual
            assert_eq!(h1.poll_count, 1);
            eprintln!("poll count: {}", h1.poll_count);

            if let DispatcherState::Normal(ref inner) = h1.inner {
                let io = inner.io.as_ref().unwrap();
                let res = &io.write_buf()[..];
                assert_eq!(
                    str::from_utf8(res).unwrap(),
                    "HTTP/1.1 100 Continue\r\n\r\n"
                );
            }

            buf.extend_read_buf("12345");
            assert!(h1.as_mut().poll(cx).is_ready());

            // polls: manual manual shutdown
            assert_eq!(h1.poll_count, 3);

            if let DispatcherState::Normal(ref inner) = h1.inner {
                let io = inner.io.as_ref().unwrap();
                let mut res = (&io.write_buf()[..]).to_owned();
                stabilize_date_header(&mut res);

                assert_eq!(
                    str::from_utf8(&res).unwrap(),
                    "\
                    HTTP/1.1 100 Continue\r\n\
                    \r\n\
                    HTTP/1.1 200 OK\r\n\
                    content-length: 5\r\n\
                    connection: close\r\n\
                    date: Thu, 01 Jan 1970 12:34:56 UTC\r\n\
                    \r\n\
                    12345\
                    "
                );
            }
        })
        .await;
    }

    #[actix_rt::test]
    async fn test_eager_expect() {
        lazy(|cx| {
            let mut buf = TestSeqBuffer::empty();
            let cfg = ServiceConfig::new(KeepAlive::Disabled, 0, 0, false, None);

            let services = HttpFlow::new(echo_path_service(), ExpectHandler, None);

            let h1 = Dispatcher::<_, _, _, _, UpgradeHandler>::new(
                buf.clone(),
                cfg,
                services,
                OnConnectData::default(),
                None,
            );

            buf.extend_read_buf(
                "\
                POST /upload HTTP/1.1\r\n\
                Content-Length: 5\r\n\
                Expect: 100-continue\r\n\
                \r\n\
                ",
            );

            futures_util::pin_mut!(h1);

            assert!(h1.as_mut().poll(cx).is_ready());
            assert!(matches!(&h1.inner, DispatcherState::Normal(_)));

            // polls: manual shutdown
            assert_eq!(h1.poll_count, 2);

            if let DispatcherState::Normal(ref inner) = h1.inner {
                let io = inner.io.as_ref().unwrap();
                let mut res = (&io.write_buf()[..]).to_owned();
                stabilize_date_header(&mut res);

                // Despite the content-length header and even though the request payload has not
                // been sent, this test expects a complete service response since the payload
                // is not used at all. The service passed to dispatcher is path echo and doesn't
                // consume payload bytes.
                assert_eq!(
                    str::from_utf8(&res).unwrap(),
                    "\
                    HTTP/1.1 100 Continue\r\n\
                    \r\n\
                    HTTP/1.1 200 OK\r\n\
                    content-length: 7\r\n\
                    connection: close\r\n\
                    date: Thu, 01 Jan 1970 12:34:56 UTC\r\n\
                    \r\n\
                    /upload\
                    "
                );
            }
        })
        .await;
    }

    #[actix_rt::test]
    async fn test_upgrade() {
        lazy(|cx| {
            let mut buf = TestSeqBuffer::empty();
            let cfg = ServiceConfig::new(KeepAlive::Disabled, 0, 0, false, None);

            let services =
                HttpFlow::new(ok_service(), ExpectHandler, Some(UpgradeHandler));

            let h1 = Dispatcher::<_, _, _, _, UpgradeHandler>::new(
                buf.clone(),
                cfg,
                services,
                OnConnectData::default(),
                None,
            );

            buf.extend_read_buf(
                "\
                GET /ws HTTP/1.1\r\n\
                Connection: Upgrade\r\n\
                Upgrade: websocket\r\n\
                \r\n\
                ",
            );

            futures_util::pin_mut!(h1);

            assert!(h1.as_mut().poll(cx).is_ready());
            assert!(matches!(&h1.inner, DispatcherState::Upgrade(_)));

            // polls: manual shutdown
            assert_eq!(h1.poll_count, 2);
        })
        .await;
    }
}
