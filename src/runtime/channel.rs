//! vredrs 通道实现
//!
//! 支持带缓冲和无缓冲的通道，用于协程间通信。

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

/// 通道内部状态
#[derive(Debug)]
struct ChannelInner<T> {
    buffer: VecDeque<T>,
    capacity: usize,
    closed: bool,
    senders: usize,
}

/// 通道发送端
#[derive(Debug, Clone)]
pub struct Sender<T: Clone> {
    inner: Arc<(Mutex<ChannelInner<T>>, Condvar)>,
}

/// 通道接收端
#[derive(Debug, Clone)]
pub struct Receiver<T: Clone> {
    inner: Arc<(Mutex<ChannelInner<T>>, Condvar)>,
}

impl<T: Clone> Sender<T> {
    /// 发送一个值到通道（可能阻塞）
    pub fn send(&self, val: T) -> Result<(), String> {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        if inner.closed {
            return Err("channel closed".to_string());
        }
        while inner.buffer.len() >= inner.capacity && inner.capacity > 0 {
            inner = cvar.wait(inner).unwrap();
            if inner.closed {
                return Err("channel closed".to_string());
            }
        }
        inner.buffer.push_back(val);
        cvar.notify_all();
        Ok(())
    }
}

impl<T: Clone> Receiver<T> {
    /// 从通道接收一个值（可能阻塞）
    pub fn recv(&self) -> Result<Option<T>, String> {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        loop {
            if let Some(val) = inner.buffer.pop_front() {
                cvar.notify_all();
                return Ok(Some(val));
            }
            if inner.closed && inner.buffer.is_empty() {
                return Ok(None);
            }
            inner = cvar.wait(inner).unwrap();
        }
    }

    /// 非阻塞接收
    pub fn try_recv(&self) -> Result<Option<T>, String> {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        let val = inner.buffer.pop_front();
        if val.is_some() { cvar.notify_all(); }
        Ok(val)
    }
}

/// 创建一对通道（发送端和接收端）
pub fn make_channel<T: Clone>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new((
        Mutex::new(ChannelInner { buffer: VecDeque::new(), capacity, closed: false, senders: 1 }),
        Condvar::new(),
    ));
    (Sender { inner: inner.clone() }, Receiver { inner })
}

impl<T: Clone> Drop for Sender<T> {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        inner.senders -= 1;
        if inner.senders == 0 {
            inner.closed = true;
        }
        cvar.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_channel_send_recv() {
        let (tx, rx) = make_channel(2);
        tx.send(42).unwrap();
        tx.send(99).unwrap();
        assert_eq!(rx.recv(), Ok(Some(42)));
        assert_eq!(rx.recv(), Ok(Some(99)));
    }

    #[test]
    fn test_channel_close() {
        let (tx, rx) = make_channel::<i64>(1);
        tx.send(1).unwrap();
        drop(tx);
        assert_eq!(rx.recv(), Ok(Some(1)));
        assert_eq!(rx.recv(), Ok(None));
    }

    #[test]
    fn test_channel_threaded() {
        let (tx, rx) = make_channel(0);
        let handle = thread::spawn(move || {
            tx.send(100).unwrap();
        });
        assert_eq!(rx.recv(), Ok(Some(100)));
        handle.join().unwrap();
    }
}

