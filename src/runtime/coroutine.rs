//! vredrs 协程调度器
//!
//! 基于协作式多任务的轻量级协程实现。
//! 使用 VecDeque 作为就绪队列，支持 spawn / yield / resume。

use std::collections::VecDeque;

/// 协程 ID
pub type CoroId = usize;

/// 协程状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoroState {
    /// 可运行
    Ready,
    /// 等待通道操作
    Waiting,
    /// 已完成
    Finished,
}

/// 协程执行体
pub type CoroBody = Box<dyn FnMut() -> CoroState + 'static>;

/// 协程结构
pub struct Coroutine {
    pub id: CoroId,
    pub state: CoroState,
    body: Option<CoroBody>,
}

impl Coroutine {
    pub fn new(id: CoroId, body: CoroBody) -> Self {
        Coroutine { id, state: CoroState::Ready, body: Some(body) }
    }

    /// 执行一步
    pub fn resume(&mut self) -> CoroState {
        if let Some(ref mut f) = self.body {
            let next = f();
            self.state = next.clone();
            if next == CoroState::Finished {
                self.body = None;
            }
            next
        } else {
            CoroState::Finished
        }
    }
}

/// 协程调度器
pub struct Scheduler {
    coroutines: Vec<Coroutine>,
    ready_queue: VecDeque<CoroId>,
    next_id: CoroId,
    running_count: usize,
}

impl Scheduler {
    pub fn new() -> Self {
        Scheduler {
            coroutines: vec![],
            ready_queue: VecDeque::new(),
            next_id: 0,
            running_count: 0,
        }
    }

    /// 创建一个新协程并加入就绪队列
    pub fn spawn<F>(&mut self, body: F) -> CoroId
    where
        F: FnMut() -> CoroState + 'static,
    {
        let id = self.next_id;
        self.next_id += 1;
        let coro = Coroutine::new(id, Box::new(body));
        self.coroutines.push(coro);
        self.ready_queue.push_back(id);
        self.running_count += 1;
        id
    }

    /// 运行调度器直到所有协程完成
    pub fn run(&mut self) {
        while self.running_count > 0 {
            if let Some(id) = self.ready_queue.pop_front() {
                if let Some(coro) = self.coroutines.get_mut(id) {
                    if coro.state != CoroState::Finished {
                        let next_state = coro.resume();
                        match next_state {
                            CoroState::Ready => {
                                self.ready_queue.push_back(id);
                            }
                            CoroState::Waiting => {
                                // 挂起，等待外部事件（如通道操作完成）
                                // 暂不重新入队，由外部 resume 恢复
                            }
                            CoroState::Finished => {
                                self.running_count -= 1;
                            }
                        }
                    }
                }
            } else {
                // 就绪队列为空但还有协程在等待 → 死锁或等待外部事件
                break;
            }
        }
    }

    /// 恢复一个等待中的协程
    pub fn resume_coro(&mut self, id: CoroId) {
        if let Some(coro) = self.coroutines.get_mut(id) {
            if coro.state == CoroState::Waiting {
                coro.state = CoroState::Ready;
                self.ready_queue.push_back(id);
            }
        }
    }

    /// 获取协程状态
    pub fn state(&self, id: CoroId) -> Option<CoroState> {
        self.coroutines.get(id).map(|c| c.state.clone())
    }

    /// 剩余活跃协程数
    pub fn active_count(&self) -> usize {
        self.running_count
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use std::cell::RefCell;

    #[test]
    fn test_spawn_and_run() {
        let mut sched = Scheduler::new();
        let count = Rc::new(RefCell::new(0));
        let c = count.clone();
        sched.spawn(move || {
            *c.borrow_mut() += 1;
            CoroState::Finished
        });
        sched.run();
        assert_eq!(*count.borrow(), 1);
        assert_eq!(sched.active_count(), 0);
    }

    #[test]
    fn test_multiple_coroutines() {
        let mut sched = Scheduler::new();
        let results = Rc::new(RefCell::new(vec![]));
        for i in 0..5 {
            let r = results.clone();
            sched.spawn(move || {
                r.borrow_mut().push(i);
                CoroState::Finished
            });
        }
        sched.run();
        assert_eq!(results.borrow().len(), 5);
    }

    #[test]
    fn test_yield_and_resume() {
        let mut sched = Scheduler::new();
        let state = Rc::new(RefCell::new(0));
        let s = state.clone();
        sched.spawn(move || {
            let mut val = s.borrow_mut();
            *val += 1;
            if *val < 3 {
                CoroState::Waiting
            } else {
                CoroState::Finished
            }
        });
        sched.run();
        assert_eq!(*state.borrow(), 1);
        sched.resume_coro(0);
        sched.run();
        assert_eq!(*state.borrow(), 2);
        sched.resume_coro(0);
        sched.run();
        assert_eq!(*state.borrow(), 3);
        assert_eq!(sched.active_count(), 0);
    }
}





