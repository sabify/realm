use std::io::{Result, ErrorKind};
use std::task::{Context, Poll, ready};
use std::marker::PhantomData;

use tokio::io::{AsyncRead, AsyncWrite};

pub struct CopyBuffer<B, S> {
    pub(crate) read_done: bool,
    pub(crate) need_flush: bool,
    pub(crate) pos: usize,
    pub(crate) cap: usize,
    pub(crate) buf: B,
    _marker: PhantomData<S>,
}

impl<B, S> CopyBuffer<B, S> {
    pub const fn new(buf: B) -> Self {
        Self {
            read_done: false,
            need_flush: false,
            pos: 0,
            cap: 0,
            buf,
            _marker: PhantomData,
        }
    }
}

pub trait AsyncIOBuf {
    type Stream: AsyncRead + AsyncWrite + Unpin;

    fn poll_read_buf(
        &mut self,
        cx: &mut Context<'_>,
        stream: &mut Self::Stream,
    ) -> Poll<Result<usize>>;

    fn poll_write_buf(
        &mut self,
        cx: &mut Context<'_>,
        stream: &mut Self::Stream,
    ) -> Poll<Result<usize>>;

    fn poll_flush_buf(
        &mut self,
        cx: &mut Context<'_>,
        stream: &mut Self::Stream,
    ) -> Poll<Result<()>>;
}

impl<B, S> CopyBuffer<B, S>
where
    B: Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
    CopyBuffer<B, S>: AsyncIOBuf,
{
    pub fn poll_copy(
        &mut self,
        cx: &mut Context<'_>,
        r: &mut <CopyBuffer<B, S> as AsyncIOBuf>::Stream,
        w: &mut <CopyBuffer<B, S> as AsyncIOBuf>::Stream,
        amt: &mut u64,
    ) -> Poll<Result<()>> {
        loop {
            // If our buffer is empty, then we need to read some data to
            // continue.
            if self.pos == self.cap && !self.read_done {
                let n = match self.poll_read_buf(cx, r) {
                    Poll::Ready(Ok(n)) => n,
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                    Poll::Pending => {
                        // Try flushing when the reader has no progress to avoid deadlock
                        // when the reader depends on buffered writer.
                        if self.need_flush {
                            ready!(self.poll_flush_buf(cx, w))?;
                            self.need_flush = false;
                        }

                        return Poll::Pending;
                    }
                };

                if n == 0 {
                    self.read_done = true;
                } else {
                    self.pos = 0;
                    self.cap = n;
                }
            }

            // If our buffer has some data, let's write it out!
            // Note: send may return ECONNRESET but splice wont, see
            // https://man7.org/linux/man-pages/man2/send.2.html
            // https://man7.org/linux/man-pages/man2/splice.2.html
            while self.pos < self.cap {
                let i = ready!(self.poll_write_buf(cx, w))?;

                if i == 0 {
                    return Poll::Ready(Err(ErrorKind::WriteZero.into()));
                } else {
                    self.pos += i;
                    *amt += i as u64;
                    self.need_flush = true;
                }
            }

            // If pos larger than cap, this loop will never stop.
            // In particular, user's wrong poll_write implementation returning
            // incorrect written length may lead to thread blocking.
            debug_assert!(
                self.pos <= self.cap,
                "writer returned length larger than input slice"
            );

            // If we've written all the data and we've seen EOF, flush out the
            // data and finish the transfer.
            if self.pos == self.cap && self.read_done {
                ready!(self.poll_flush_buf(cx, w))?;
                return Poll::Ready(Ok(()));
            }
        }
    }
}
