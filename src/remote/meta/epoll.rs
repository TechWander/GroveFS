//! meta-server 挂载面 / 治理面的 **epoll 事件循环**，替代「一连接一线程」。
//!
//! 形状照 treedb 仓 `src/net/server.rs` 的单线程事件循环（`epoll_create` / `epoll_set`
//! / `epoll_mod` / 轮末统一写出 / 读侧背压摘 `EPOLLIN`），因为要解决的是同一件事：绝大
//! 多数 socket 在绝大多数时间里没有任何事可做，而线程模型让每一个「没事可做」都占一条
//! OS 线程 + 8 MiB 栈。挂载数一上去，付的是虚存、调度与 `threads-max`，不是吞吐。
//!
//! 有一处**必须与 treedb 不同**，不能照搬：本层的请求处理本身就是阻塞的——引擎往返
//! （treedb round-trip / SQLite / PG）、`--witness` 的仲裁器同步投票、`--sync-repl` 的推
//! 备库，都可能是几十毫秒到秒级的阻塞调用。treedb 敢把 op 摊在事件循环里，是因为它唯一
//! 的阻塞（fsync）交给了独立的 committer 线程；这里没有那样干净的分界。后两类 HA 阻塞
//! 已收敛成有界预算（仲裁扇出提前收口 + 单飞、推送槽与 connect 各有上限，超限即降级），
//! 所以「worker 可以下场阻塞」才是个成立的假设——预算与残差见
//! `docs/adr/0005-write-path-blocking-budget.md`。引擎往返那一类仍靠下面的背压兜。所以形状是：
//!
//! ```text
//!   循环线程（LOOPS 条，各自一个 epoll）
//!     └─ 独占 socket：accept / read / 帧切分 / 写队列 flush / 连接生命周期 / 超时
//!          攒出一个完整请求帧 ──► 有界 worker 通道（GROVEFS_META_WORKERS 条，默认 8）
//!                                   └─ 只做后端调用，应答帧塞回该连接的写队列
//! ```
//!
//! 三条不变量，也是这套改造的全部意义：
//!
//! 1. **循环线程从不下场处理请求**，只做 `read`/`write`/`epoll_*`。它对 [`Inner`] 的加锁
//!    只覆盖内存操作（入队 / 出队 / 拷贝一帧），任何系统调用都不在锁内发生——否则一个
//!    慢后端调用就能把整条循环上的所有连接钉住，那比原模型更糟。
//! 2. **worker 从不碰 socket**：入参是切好的 `(op, body)`，出参是帧字节，连接只经
//!    [`Handle`] 的队列访问。`sock`/`buf` 两个字段住在 [`Conn`] 本体而不是 `Inner` 里，
//!    所以这条是类型层面的事实，不靠自觉。于是 worker 数 = 同时在飞的后端调用数，与连接
//!    数无关。
//! 3. **一条连接同时最多一个在飞请求**：协议每连接严格 FIFO、没有 request id
//!    （`remote/proto.rs` 的契约），`busy` 未清时新帧只排队。今天一连接一线程本来就是串行
//!    的，这里只是把既有约束写进类型。
//!
//! 收益落在**空闲**上：挂上不写的挂载、还没到点的续租、订阅连接、半开的对端，从「一条线
//! 程」变成「一份缓冲区」。订阅路径尤其值——原来为了等一次 `read` 返回 EOF 而永久挂着线
//! 程（`server_ext.rs` 的 `serve_subscriber`），现在读端就是 epoll 本身。
//!
//! 只在 Linux + `native` 编译：`libc` 是 optional 依赖，remote-only / Windows 客户端构建
//! 绝不能碰到本模块（仓内 platform.rs 分层的既有约束）。非 Linux、或显式
//! `GROVEFS_META_THREADS=1` 时，走 `server_ext` 里保留的原线程路径。
//!
//! 每个端口（挂载面、治理面）各起一组：`spawn` 一次 = LOOPS 条循环 + 一档 worker 池。
//! 两面本来就是不同的信任边界（firewall 可分），共用循环只会让治理面的慢调用去拖累
//! 挂载面。测试里一个进程会起多个 meta-server，所以池是 `spawn` 的局部物，不是全局量。

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::io::{self, IoSlice, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::server_ext::MetaRuntime;

/// 每轮 `epoll_wait` 最多取回的事件数（treedb 同值）。
const MAX_EVENTS: usize = 1024;
/// `epoll_wait` 超时（毫秒）。到点只做收尾（超时裁决 + 脏队列排空）。worker 产出应答另有
/// wake eventfd 即时叫醒，所以这个值只决定收尾粒度，不决定延迟。
const TICK: i32 = 200;
/// 单循环每轮最多 accept 的条数：backlog 里可能一次来很多，但不能让 accept 饿死已有连接
/// 的读写。
const ACCEPT_BURST: usize = 64;
/// 帧体上限。协议层的 `proto::MAX_FRAME`（256 MiB）管「别把服务器撑爆」，这里的 8 MiB 管
/// 另一头：挂载面的请求帧都很小（最大的是批量属性与目录页），一个永远攒不齐的大帧头不该
/// 把缓冲区养到 256 MiB。数据面不在本循环里。
const READ_CAP: usize = 8 << 20;
/// 读缓冲高水位：到了就先摘 `EPOLLIN`（帧多半没攒齐），下一轮再看。
const READ_HOLD: usize = 6 << 20;
/// 单连接写队列上限（帧数 / 字节数），任一越界即断连。今天的等价物是
/// `BROADCAST_SEND_TIMEOUT`：发送阻塞 2s 后 broadcast 把这个订阅者摘掉。语义保持不变（推
/// 的是失效事件，客户端本来就会靠属性 TTL 收敛），只是从「拖慢写线程 2 秒」变成「不拖累
/// 任何人，直接摘」。
const OUT_FRAMES: usize = 512;
const OUT_BYTES: usize = 16 << 20;
/// 单连接待派发队列上限。线程模型下这个背压是 TCP 窗口自带的（没读走的字节留在内核
/// 里），换成先切帧再排队就必须显式给一个，否则一个发疯的客户端可以把服务端的内存吃掉。
/// 实际不会靠近这个数：每连接一问一答，客户端不在同一个 socket 上管线发送
/// （`remote/proto.rs` 的契约），越限只可能是协议滥用。
const IN_FRAMES: usize = 512;
/// 攒不齐一整帧的容忍时间。今天的阻塞 `read` 对半截帧没有上限——收到帧头第一个字节之后
/// 那条线程可以挂到天荒地老，keepalive 也管不了「慢速发送」。这里补一个显式截止；凑齐一
/// 帧就刷新，正常客户端不受影响。
const PARTIAL_DEADLINE: Duration = Duration::from_secs(60);
/// 治理面 pinned 会话的空洞上限，与既有 `GOV_PINNED_IDLE` 同值：那条路径今天就靠 socket
/// 读超时实现，这里把它搬进循环执行，不改语义。工作连接与订阅连接**故意不裁空闲**——它们
/// 的半开回收走 keepalive（2026-11-17 固化的既有语义）。
const GOV_IDLE: Duration = Duration::from_secs(300);
/// worker 数默认值 = 同时在飞的后端调用上限。8 与一挂载 fuser `--threads` 默认 4 同量级，
/// 几十倍挂载下仍然够，且远低于任何后端的并发承受线。`GROVEFS_META_WORKERS` 覆盖。
const DEFAULT_WORKERS: usize = 8;
/// 每端口的事件循环条数。>1 才吃得满多核的 accept 与读写；连接归属由 accept 落点决定，
/// 循环之间不需要协调。对外可见只为让 `serve_runtime` 启动时能报出真实形状。
pub const LOOPS: usize = 2;
/// 派发队列满时的等待上限。宁可一次请求失败让客户端的池重拨，也不要无界排队把延迟拖到
/// 不可控——与 treedb `--max-conn` 满员即「接受后立刻关掉」同一取舍。
const DISPATCH_WAIT: Duration = Duration::from_millis(50);
/// 单次 `read` 的搬运块大小。
const READ_CHUNK: usize = 32 * 1024;

// ---------------------------------------------------------------------------
// epoll 原语
// ---------------------------------------------------------------------------

fn last_os() -> io::Error {
    io::Error::last_os_error()
}

fn epoll_create() -> io::Result<RawFd> {
    let ep = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if ep < 0 {
        return Err(last_os());
    }
    Ok(ep)
}

/// 首次注册。ADD 撞 EEXIST 退化成 MOD：一条 fd 的「第一次挂」与「已经挂着」都可能走到这
/// 里，判别的代价比多一次 `epoll_ctl` 高。
fn epoll_set(ep: RawFd, fd: RawFd, want_write: bool) -> io::Result<()> {
    let mut ev = unsafe { std::mem::zeroed::<libc::epoll_event>() };
    ev.events = libc::EPOLLIN as u32 | if want_write { libc::EPOLLOUT as u32 } else { 0 };
    // `epoll_event.data` is a union; libc flattens it to a `u64` field that covers the
    // `fd` member (an fd never exceeds `i32`).
    ev.u64 = fd as u64;
    if unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, fd, &mut ev) } < 0
        && unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_MOD, fd, &mut ev) } < 0
    {
        return Err(last_os());
    }
    Ok(())
}

/// 纯 MOD（fd 必须已注册）。与 `epoll_set` 的差别就是不拿 ADD 去撞 EEXIST。
/// `want_read = false` 只出现在读侧背压：epoll 是**电平触发**的，缓冲到水位时我们主动不
/// 读，若不把 `EPOLLIN` 摘掉，`epoll_wait` 每轮都会立刻把同一个 fd 报回来空转。
fn epoll_mod(ep: RawFd, fd: RawFd, want_write: bool, want_read: bool) -> io::Result<()> {
    let mut ev = unsafe { std::mem::zeroed::<libc::epoll_event>() };
    ev.events = if want_read { libc::EPOLLIN as u32 } else { 0 }
        | if want_write { libc::EPOLLOUT as u32 } else { 0 };
    ev.u64 = fd as u64;
    if unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_MOD, fd, &mut ev) } < 0 {
        return Err(last_os());
    }
    Ok(())
}

fn ev_wake() -> io::Result<RawFd> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(last_os());
    }
    Ok(fd)
}

fn drain_eventfd(fd: RawFd) {
    let mut buf = [0u8; 8];
    let _ = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
}

fn poke_eventfd(fd: RawFd) {
    let one: [u8; 8] = 1u64.to_le_bytes();
    let _ = unsafe { libc::write(fd, one.as_ptr() as *const libc::c_void, one.len()) };
}

// ---------------------------------------------------------------------------
// 连接
// ---------------------------------------------------------------------------

/// 连接扮演哪种角色。首帧判角色，与今天 `handle_meta_conn` 的写法一致；治理端口的首帧
/// 必须是 `OP_CONTROL`，所以 `from_gov` 在 accept 落点就定了。
///
/// 与 `server_ext::Role`（Primary / Standby，HA 角色）无关，故特意叫 `ConnRole`。
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum ConnRole {
    /// 还没看过首帧。
    Handshake,
    /// 挂载面工作连接（`M_OPEN` 之后的元数据请求）。
    Work,
    /// 订阅者：只出不进，帧由别的线程塞进写队列。
    Push,
    /// 对端复制 / 心跳控制连接。
    Peer,
    /// 治理面。
    Gov,
}

/// worker 可见的连接状态。循环也读它，但只在内存操作的极短临界区里（不变量 1）。
struct Inner {
    /// 已切好、待派发的完整请求帧。协议一问一答，长度实践上不是 0 就是 1。
    q: VecDeque<(u8, Vec<u8>)>,
    /// 待写出的完整应答 / 事件帧。
    out: VecDeque<Vec<u8>>,
    role: ConnRole,
    /// 有一个请求正在 worker 手上。
    busy: bool,
    /// 步骤函数判死：写队列排空后收尾。应答不能因为「处理完要关」被丢在队列里，所以
    /// 不是立刻摘除，而是先写干净（见 [`EventLoop::retire`]）。
    dying: bool,
    /// 订阅登记（id + 注册的桶名），连接结束时据此摘 `subs`。
    sub: Option<(u64, Vec<String>)>,
    /// 这条连接的会话（`MetaSession` / `GovSession`）。类型对本模块未知，故擦除；不变量
    /// 3 保证同一时刻只有一个 worker 取得走它，所以「取出—处理—放回」不需要额外同步。
    sess: Option<Box<dyn Any + Send>>,
    /// 最近一次收到完整帧的时刻，治理面据此执行 [`GOV_IDLE`]。
    last_frame: Instant,
    /// `buf` 里有半截帧时的截止时刻。
    partial: Option<Instant>,
    /// epoll 掩码里当前真的挂着 `EPOLLOUT` / 摘掉了 `EPOLLIN` 吗。与目标态一致时一次
    /// `epoll_ctl` 都不付——小应答恰恰几乎永远本轮就写完。
    masked_write: bool,
    held_read: bool,
}

/// 一条连接。`sock` 与 `buf` 只属于它所在的循环线程（不变量 1），故放在 `Conn` 而不是
/// `Inner`：worker 拿不到它们。
///
/// 生命周期由 `Arc` 决定：循环摘掉两份引用后，若还有 worker 持着句柄，fd 会晚一点才关；
/// 那时全局表里已查不到这条连接，worker 的写出被静默丢弃——正是想要的结果（客户端已走）。
pub struct Conn {
    sock: TcpStream,
    fd: RawFd,
    /// 本连接唯一 id。fd 号会被复用，跨「摘除—重连」窗口的迟到写出必须靠它挡住。
    id: u64,
    inner: Mutex<Inner>,
    /// 已收到、尚未凑成整帧的字节。仅循环线程读写（故在 `Conn` 不在 `Inner`），但连接本体
    /// 是 `Arc`，所以下内部可变。加锁顺序永远 `buf` 先、`inner` 后，以免与 `sync_interest`
    /// 互相反方向拿锁。
    buf: Mutex<Vec<u8>>,
}

/// 全局连接表 + 每循环的脏队列与 wake fd。循环与 worker 唯一的交汇点就在这里，
/// 且每次持锁都只是哈希查找与队列进出。
///
/// 脏队列**按循环分开**：一条连接只由 accept 它的那个循环写 socket，若共用一个队列，
/// 别的循环会把不属于自己的 fd 从队列里取走并丢弃，那条连接就再也等不到 EPOLLOUT。
struct Shared {
    conns: Mutex<HashMap<RawFd, (u64, Arc<Conn>)>>,
    dirty: Vec<Mutex<Vec<RawFd>>>,
    wakes: Vec<RawFd>,
    next_id: AtomicU64,
    pool: Pool,
}

impl Shared {
    fn find(&self, fd: RawFd, id: u64) -> Option<Arc<Conn>> {
        self.conns
            .lock()
            .unwrap()
            .get(&fd)
            .filter(|(cid, _)| *cid == id)
            .map(|(_, c)| c.clone())
    }

    /// worker 的「这条连接有东西要写」：入它所属循环的脏队列 + 叫醒那条循环。
    fn mark_dirty(&self, fd: RawFd, loop_id: usize) {
        self.dirty[loop_id].lock().unwrap().push(fd);
        poke_eventfd(self.wakes[loop_id]);
    }
}

/// worker 手里的连接句柄：能读请求、能写应答、能改角色，碰不到 socket。
#[derive(Clone)]
pub struct Handle {
    shared: Arc<Shared>,
    fd: RawFd,
    id: u64,
    loop_id: usize,
    from_gov: bool,
}

impl Handle {
    fn conn(&self) -> Option<Arc<Conn>> {
        self.shared.find(self.fd, self.id)
    }

    /// 这条连接来自哪个端口（治理面的首帧必须是 `OP_CONTROL`）。判据是循环的属性
    /// 而非连接的属性——一条循环只听一个端口，所以 accept 落点即答案，不必查表。
    pub fn gov_plane(&self) -> bool {
        self.from_gov
    }

    pub fn role(&self) -> Option<ConnRole> {
        self.conn().map(|c| c.inner.lock().unwrap().role)
    }

    pub fn set_role(&self, r: ConnRole) {
        if let Some(c) = self.conn() {
            c.inner.lock().unwrap().role = r;
        }
    }

    /// 取出会话（连接已摘、或类型不对时 `None`）。锁在返回前就放掉，所以下一步的
    /// `sink()` 重新加锁不会自死锁。
    pub fn take_session<T: Any + Send>(&self) -> Option<T> {
        let c = self.conn()?;
        let mut g = c.inner.lock().unwrap();
        g.sess
            .take()
            .and_then(|b| b.downcast::<T>().ok())
            .map(|b| *b)
    }

    pub fn put_session<T: Any + Send>(&self, v: T) {
        if let Some(c) = self.conn() {
            c.inner.lock().unwrap().sess = Some(Box::new(v));
        }
    }

    /// 订阅登记完成后记下 (id, buckets)：角色切成只出不进，连接被摘时据此清 `subs`。
    pub fn note_sub(&self, id: u64, buckets: Vec<String>) {
        if let Some(c) = self.conn() {
            let mut g = c.inner.lock().unwrap();
            g.sub = Some((id, buckets));
            g.role = ConnRole::Push;
        }
    }

    /// 追加一帧应答/事件。`false` = 连接已摘，或写队列越限（客户端不再收）——调用方应当
    /// 就此收手，与今天 broadcast 摘掉不收敛的订阅者同语义。
    pub fn push(&self, frame: Vec<u8>) -> bool {
        let Some(c) = self.conn() else { return false };
        let ok = {
            let mut g = c.inner.lock().unwrap();
            let bytes: usize = g.out.iter().map(|f| f.len()).sum();
            if g.out.len() >= OUT_FRAMES || bytes > OUT_BYTES {
                false
            } else {
                g.out.push_back(frame);
                true
            }
        };
        if ok {
            self.shared.mark_dirty(self.fd, self.loop_id);
        } else {
            self.kill();
        }
        ok
    }

    /// 步骤函数判死：先把已排好的应答写完，再由循环收尾。
    pub fn kill(&self) {
        let Some(c) = self.conn() else { return };
        c.inner.lock().unwrap().dying = true;
        self.shared.mark_dirty(self.fd, self.loop_id);
    }

    pub fn is_dying(&self) -> bool {
        self.conn().is_none_or(|c| c.inner.lock().unwrap().dying)
    }

    /// 一次应答的暂存出口。写出的字节在 [`FrameSink::finish`] 之前不落队列，于是「一帧」
    /// 的边界由编码函数决定，而不是由 socket 的短写决定——非阻塞 socket 上把一帧劈成两半
    /// 是不可接受的（协议没有分帧边界之外的续传概念）。
    pub fn sink(&self) -> FrameSink<'_> {
        FrameSink {
            h: self,
            buf: Vec::new(),
        }
    }

    /// 取这条连接的下一帧；没有就交还派发权（清 `busy`）。取帧与清位在同一把锁里完成，
    /// 所以「队列非空但没人处理」的窗口不存在（循环的派发走的是同一把锁）。
    fn next_frame(&self) -> Option<(u8, Vec<u8>)> {
        let c = self.conn()?;
        let mut g = c.inner.lock().unwrap();
        if g.dying {
            g.q.clear();
            g.busy = false;
            return None;
        }
        let next = g.q.pop_front();
        if next.is_none() {
            g.busy = false;
        }
        next
    }
}

/// [`Handle::sink`]：一次应答的暂存区，`io::Write` 的形状让 `server_ext` 里
/// `proto::send_response(&mut w, ..)` 的既有写法不必改一个字。
pub struct FrameSink<'a> {
    h: &'a Handle,
    buf: Vec<u8>,
}

impl Write for FrameSink<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }
    /// 必须自己实现：`io::Write` 的默认 `write_vectored` 只消费第一个非空 slice，而
    /// `proto::send_framed` 正是拿 `write_vectored` 循环推进的——不覆盖它，帧头会被
    /// 反复追加、负载永远进不来（一个静默的协议损坏）。
    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let n = bufs.iter().map(|b| b.len()).sum();
        for b in bufs {
            self.buf.extend_from_slice(b);
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl FrameSink<'_> {
    /// 把攒到的字节作为一个写队列项入队；之后由循环负责写 socket。一帧、两帧合一都不影
    /// 响正确性——线上是自描述的分帧，客户端按头切。
    pub fn finish(&mut self) -> io::Result<()> {
        let frame = std::mem::take(&mut self.buf);
        if frame.is_empty() {
            return Ok(());
        }
        if self.h.push(frame) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "meta epoll: connection gone or write queue full",
            ))
        }
    }
}

/// 步骤函数：处理一帧，产出应答，可能更新角色。签名里没有 socket。
/// `Err`/`Ok(())` 之后的收尾由 `Handle::kill` 显式表达——步骤函数想断连就 `kill`，
/// 想继续就返回 `Ok(())`；返回 `Err` 视同 `kill`（与今天 `handle_meta_conn` 用错误
/// 结束一条连接一致）。
pub type Step = Arc<dyn Fn(&Arc<MetaRuntime>, &Handle, u8, &[u8]) -> io::Result<()> + Send + Sync>;

// ---------------------------------------------------------------------------
// worker 池
// ---------------------------------------------------------------------------

struct Job {
    h: Handle,
    op: u8,
    body: Vec<u8>,
}

/// 一档 worker。`spawn`  each 端口一份，所以测试里多个 meta-server 不会共用一条通道、
/// 更不会拿到别人的 `Arc<MetaRuntime>`。
struct Pool {
    tx: Vec<mpsc::SyncSender<Job>>,
    rr: AtomicU64,
}

pub fn worker_count() -> usize {
    std::env::var("GROVEFS_META_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| (1..=1024).contains(n))
        .unwrap_or(DEFAULT_WORKERS)
}

impl Pool {
    fn start(rt: &Arc<MetaRuntime>, step: &Step, workers: usize, depth: usize) -> Pool {
        let mut tx = Vec::with_capacity(workers);
        for k in 0..workers {
            let (s, r) = mpsc::sync_channel::<Job>(depth);
            tx.push(s);
            let rt = rt.clone();
            let step = step.clone();
            thread::Builder::new()
                .name(format!("meta-worker{k}"))
                .spawn(move || {
                    for job in r {
                        Pool::serve(&rt, &step, job);
                    }
                })
                .expect("spawn meta worker");
        }
        Pool {
            tx,
            rr: AtomicU64::new(0),
        }
    }

    /// 处理一个请求帧，然后接着处理这条连接上已经攒到的下一帧：客户端在管线发送时省掉
    /// 一次「归还—再派发」的通道往返。所有等待都在锁外。
    fn serve(rt: &Arc<MetaRuntime>, step: &Step, job: Job) {
        let h = job.h;
        let mut op = job.op;
        let mut body = job.body;
        loop {
            if h.is_dying() {
                break;
            }
            if (step)(rt, &h, op, &body).is_err() {
                // 编码失败/后端抛错：不再读这条连接的后续帧，让循环收尾。
                h.kill();
                break;
            }
            match h.next_frame() {
                Some((o, b)) => {
                    op = o;
                    body = b;
                }
                None => break,
            }
        }
    }

    /// 轮转派发。满则等一小段，还满就报失败，由循环断这条连接。
    fn send(&self, h: &Handle, op: u8, mut body: Vec<u8>) -> bool {
        let n = self.tx.len();
        if n == 0 {
            return false;
        }
        let start = Instant::now();
        let mut tries = 0usize;
        loop {
            let k = (self.rr.fetch_add(1, Ordering::Relaxed) as usize) % n;
            match self.tx[k].try_send(Job {
                h: h.clone(),
                op,
                body,
            }) {
                Ok(()) => return true,
                Err(mpsc::TrySendError::Full(j)) => {
                    body = j.body;
                    tries += 1;
                    // 每个 worker 通道都撞过一次、且已超过 DISPATCH_WAIT 才放弃。
                    if tries >= n && start.elapsed() >= DISPATCH_WAIT {
                        return false;
                    }
                    thread::sleep(Duration::from_micros(150));
                }
                // 池已随进程退出而解散：没有「等一下就好」这回事。
                Err(mpsc::TrySendError::Disconnected(j)) => {
                    let _ = j;
                    return false;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 事件循环
// ---------------------------------------------------------------------------

struct EventLoop {
    ep: RawFd,
    /// 本循环监听的端口（accept 落点即 `from_gov` 的判据）。
    listen_fd: RawFd,
    from_gov: bool,
    wake_fd: RawFd,
    id: usize,
    conns: HashMap<RawFd, Arc<Conn>>,
    events: Vec<libc::epoll_event>,
    shared: Arc<Shared>,
    rt: Arc<MetaRuntime>,
    buf: [u8; READ_CHUNK],
}

/// 起 `LOOPS` 条循环 + 一档 worker 池，返回循环线程的 join 句柄。
///
/// 不接管调用者的 listener：每条循环自己 `try_clone` 一份并交出 fd 所有权，所以本函数
/// 返回 `Err` 时 listener 原封不动，`serve_runtime` 可以退回线程形状。成功后调用者丢掉
/// 自己的那份也没关系——dup 出来的 fd 指向同一个 open file description，socket 不会关。
///
/// 两个可能失败的步骤（建唤醒 fd、dup listener）都排在任何线程起起来之前：一旦已经
/// 有循环在 accept，再返回 `Err` 会让调用方也起一个 accept 循环，两条路同时抢同一个
/// 端口。调用方拿到句柄后必须 join：循环就是服务本体，`spawn` 返回不等于“可以退场”。
pub fn spawn(
    listener: &TcpListener,
    from_gov: bool,
    rt: Arc<MetaRuntime>,
    step: Step,
) -> io::Result<Vec<thread::JoinHandle<()>>> {
    let workers = worker_count();
    let depth = (workers * 64).max(256);
    let mut wake_fds = Vec::with_capacity(LOOPS);
    for _ in 0..LOOPS {
        match ev_wake() {
            Ok(fd) => wake_fds.push(fd),
            Err(e) => {
                for fd in wake_fds {
                    let _ = unsafe { libc::close(fd) };
                }
                return Err(e);
            }
        }
    }
    let mut clones = Vec::with_capacity(LOOPS);
    for _ in 0..LOOPS {
        match listener.try_clone() {
            Ok(l) => clones.push(l),
            Err(e) => {
                for fd in wake_fds {
                    let _ = unsafe { libc::close(fd) };
                }
                return Err(e);
            }
        }
    }
    let shared = Arc::new(Shared {
        conns: Mutex::new(HashMap::new()),
        dirty: (0..LOOPS).map(|_| Mutex::new(Vec::new())).collect(),
        wakes: wake_fds.clone(),
        next_id: AtomicU64::new(1),
        pool: Pool::start(&rt, &step, workers, depth),
    });
    let plane = if from_gov { "control" } else { "meta" };
    let mut joins = Vec::with_capacity(LOOPS);
    for (id, (wake_fd, l)) in wake_fds.into_iter().zip(clones).enumerate() {
        let shared = shared.clone();
        let rt = rt.clone();
        joins.push(
            thread::Builder::new()
                .name(format!("{plane}-loop{id}"))
                .spawn(move || {
                    let mut lp = match EventLoop::new(l, from_gov, id, wake_fd, shared, rt) {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("meta epoll: {plane} loop {id} failed to start: {e}");
                            return;
                        }
                    };
                    if let Err(e) = lp.run() {
                        eprintln!("meta epoll: {plane} loop {id} exited: {e}");
                    }
                })
                .expect("spawn meta event loop"),
        );
    }
    Ok(joins)
}

impl EventLoop {
    fn new(
        listener: TcpListener,
        from_gov: bool,
        id: usize,
        wake_fd: RawFd,
        shared: Arc<Shared>,
        rt: Arc<MetaRuntime>,
    ) -> io::Result<Self> {
        let ep = epoll_create()?;
        listener.set_nonblocking(true)?;
        let listen_fd = listener.into_raw_fd();
        epoll_set(ep, listen_fd, false)?;
        epoll_set(ep, wake_fd, false)?;
        Ok(Self {
            ep,
            listen_fd,
            from_gov,
            wake_fd,
            id,
            conns: HashMap::new(),
            events: vec![unsafe { std::mem::zeroed() }; MAX_EVENTS],
            shared,
            rt,
            buf: [0u8; READ_CHUNK],
        })
    }

    fn run(&mut self) -> io::Result<()> {
        loop {
            let n = unsafe {
                libc::epoll_wait(
                    self.ep,
                    self.events.as_mut_ptr(),
                    self.events.len() as i32,
                    TICK,
                )
            };
            if n < 0 {
                let e = last_os();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            for i in 0..n as usize {
                let fd = self.events[i].u64 as RawFd;
                let bits = self.events[i].events;
                if fd == self.listen_fd {
                    self.accept();
                    continue;
                }
                if fd == self.wake_fd {
                    drain_eventfd(fd);
                    continue;
                }
                let err_hup = bits & (libc::EPOLLHUP as u32 | libc::EPOLLERR as u32) != 0;
                let readable = bits & libc::EPOLLIN as u32 != 0 || err_hup;
                let writable = bits & libc::EPOLLOUT as u32 != 0;
                // 一条事件里读写可能同时成立（HUP + 还有东西要写）：先写后读，写的失败
                // 不该掩盖读侧的收尾判定。
                if writable {
                    self.on_writable(fd);
                }
                if readable && self.conns.contains_key(&fd) {
                    self.on_readable(fd);
                }
            }
            self.flush_dirty();
            self.sweep();
        }
    }

    fn accept(&mut self) {
        for _ in 0..ACCEPT_BURST {
            let raw = unsafe {
                libc::accept4(
                    self.listen_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                )
            };
            if raw < 0 {
                let e = last_os();
                match e.raw_os_error() {
                    // EAGAIN == EWOULDBLOCK on Linux: backlog already drained, normal end.
                    Some(libc::EAGAIN) => {}
                    // fd 耗尽：比线程模型温和得多（那时先撞的是线程上限），但必须显式记一
                    // 笔——静默丢连接是最难查的故障形状。
                    Some(libc::EMFILE) | Some(libc::ENFILE) => {
                        eprintln!("meta accept failed: {e} (fd limit; raise LimitNOFILE)")
                    }
                    _ => eprintln!("meta accept failed: {e}"),
                }
                return;
            }
            // `accept4` 成功才包 TcpStream：失败返回 -1，`from_raw_fd(-1)` 是 UB。
            let sock = unsafe { TcpStream::from_raw_fd(raw) };
            let fd = sock.as_raw_fd();
            if let Err(e) = Conn::tune(&sock) {
                eprintln!("meta epoll: tune accepted socket: {e}");
                continue;
            }
            if let Err(e) = epoll_set(self.ep, fd, false) {
                eprintln!("meta epoll: register fd {fd}: {e}");
                continue;
            }
            let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
            let c = Arc::new(Conn {
                sock,
                fd,
                id,
                inner: Mutex::new(Inner {
                    q: VecDeque::new(),
                    out: VecDeque::new(),
                    role: ConnRole::Handshake,
                    busy: false,
                    dying: false,
                    sub: None,
                    sess: None,
                    last_frame: Instant::now(),
                    partial: None,
                    masked_write: false,
                    held_read: false,
                }),
                // 已收到、尚未凑成整帧的字节。仅循环线程读写，但连接是 `Arc`，所以走
                // 内部可变——与 `inner` 一样，只在循环线程的短临界区里持有（不变量 1），
                // 且加锁顺序永远是 `buf` 先、`inner` 后。
                buf: Mutex::new(Vec::new()),
            });
            self.conns.insert(fd, c.clone());
            self.shared.conns.lock().unwrap().insert(fd, (id, c));
        }
    }

    fn on_readable(&mut self, fd: RawFd) {
        let Some(c) = self.conns.get(&fd).cloned() else {
            return;
        };
        let mut frames: Vec<(u8, Vec<u8>)> = Vec::new();
        let mut dead = !self.read_into(&c);
        // 无论读侧是否判死（`READ_CAP` 触顶、对端已关），已到位的整帧都要先切出来：半开
        // 关闭的最后一帧应答还得发出去。切帧顺带刷新 `last_frame` / 清半截帧截止。
        if c.take_frames(&mut frames).is_err() {
            dead = true;
        }
        if dead {
            self.retire(&c);
            return;
        }
        let mut drop_now = false;
        for (op, body) in frames {
            if !self.dispatch(&c, op, body) {
                drop_now = true;
                break;
            }
        }
        if drop_now {
            self.retire(&c);
        } else if let Err(e) = self.sync_interest(&c) {
            eprintln!("meta epoll: interest fd {fd}: {e}");
            self.retire(&c);
        }
    }

    /// 读进本线程独占的 `buf`。电平触发下不必读到 EAGAIN（下一轮还会报），但读满水位就
    /// 先回去切帧、下一轮再看。返回 `false` = 对端已关或出错。
    ///
    /// socket 走 `&TcpStream` 的 `Read` 实现（std 对 `&TcpStream` 也实现了 `Read`/`Write`），
    /// 所以连接可以留在 `Arc` 里不必 `ArcMut`。
    fn read_into(&mut self, c: &Conn) -> bool {
        let mut sock: &TcpStream = &c.sock;
        loop {
            let room = {
                let b = c.buf.lock().unwrap();
                if b.len() >= READ_CAP {
                    return false;
                }
                (READ_HOLD - b.len()).min(self.buf.len())
            };
            if room == 0 {
                // 到水位：外层会切一次帧，下一轮再由 `sync_interest` 摘着 `EPOLLIN`。
                return true;
            }
            match sock.read(&mut self.buf[..room]) {
                Ok(0) => return false,
                Ok(n) => {
                    c.buf.lock().unwrap().extend_from_slice(&self.buf[..n]);
                    let mut g = c.inner.lock().unwrap();
                    if g.partial.is_none() {
                        g.partial = Some(Instant::now() + PARTIAL_DEADLINE);
                    }
                    drop(g);
                    if n < room {
                        return true;
                    }
                }
                Err(e) => match e.kind() {
                    io::ErrorKind::WouldBlock => return true,
                    io::ErrorKind::Interrupted => continue,
                    _ => return false,
                },
            }
        }
    }

    /// 把一帧交给 worker。同连接已在飞时只排队（协议每连接 FIFO）。返回 `false` = 派发
    /// 不上（worker 队列排满）或已判死，调用方收尾。
    fn dispatch(&mut self, c: &Conn, op: u8, body: Vec<u8>) -> bool {
        let take = {
            let mut g = c.inner.lock().unwrap();
            if g.dying {
                return false;
            }
            // 订阅连接只出不进：客户端在订阅后不该再发请求（今天 `serve_subscriber`
            // 也是收到就丢）。留在循环里丢，比让 worker 空跑一趟便宜。
            if g.role == ConnRole::Push {
                return true;
            }
            if g.q.len() >= IN_FRAMES {
                return false;
            }
            g.q.push_back((op, body));
            if g.busy {
                None
            } else {
                g.busy = true;
                g.q.pop_front()
            }
        };
        let Some((op, body)) = take else {
            return true;
        };
        let h = Handle {
            shared: self.shared.clone(),
            fd: c.fd,
            id: c.id,
            loop_id: self.id,
            from_gov: self.from_gov,
        };
        if self.shared.pool.send(&h, op, body) {
            return true;
        }
        c.inner.lock().unwrap().busy = false;
        eprintln!("meta epoll: worker queue full, dropping fd {}", c.fd);
        false
    }

    fn on_writable(&mut self, fd: RawFd) {
        let Some(c) = self.conns.get(&fd).cloned() else {
            return;
        };
        if !self.flush(&c) {
            self.retire(&c);
            return;
        }
        if self.sync_interest(&c).is_err() {
            self.retire(&c);
        }
    }

    /// 把写队列刷进 socket。`false` = 连接已死。`write` 在锁**外**：锁只护住「取队首的
    /// 副本」与「按实际写出的字节裁剪队列」这两段内存操作（不变量 1）。
    fn flush(&mut self, c: &Conn) -> bool {
        let mut sock: &TcpStream = &c.sock;
        loop {
            let front = {
                let g = c.inner.lock().unwrap();
                g.out.front().cloned()
            };
            let Some(front) = front else { return true };
            match sock.write(&front[..]) {
                Ok(0) => return false,
                Ok(n) => {
                    let mut g = c.inner.lock().unwrap();
                    if n >= front.len() {
                        g.out.pop_front();
                    } else if let Some(f) = g.out.front_mut() {
                        f.drain(..n);
                    }
                    if n < front.len() {
                        return true; // 还会报 EPOLLOUT
                    }
                }
                Err(e) => match e.kind() {
                    io::ErrorKind::WouldBlock => return true,
                    io::ErrorKind::Interrupted => continue,
                    _ => return false,
                },
            }
        }
    }

    fn flush_dirty(&mut self) {
        let pending = {
            let mut g = self.shared.dirty[self.id].lock().unwrap();
            if g.is_empty() {
                return;
            }
            std::mem::take(&mut *g)
        };
        for fd in pending {
            let Some(c) = self.conns.get(&fd).cloned() else {
                continue;
            };
            if c.inner.lock().unwrap().out.is_empty() {
                // 判死且已写干净：收尾。
                if c.inner.lock().unwrap().dying {
                    self.retire(&c);
                }
                continue;
            }
            if !self.flush(&c) {
                self.retire(&c);
                continue;
            }
            if self.sync_interest(&c).is_err() {
                self.retire(&c);
            }
        }
    }

    /// 把掩码对齐到「有东西要写 + 读侧允许继续」。与当前记录一致时一次 `epoll_ctl` 不付。
    fn sync_interest(&self, c: &Conn) -> io::Result<()> {
        // `buf` 先于 `inner`：与 `read_into`/`take_frames` 同一方向，不会反序互锁。
        let held_read = c.buf.lock().unwrap().len() >= READ_HOLD;
        let mut g = c.inner.lock().unwrap();
        let want_write = !g.out.is_empty();
        if g.masked_write != want_write || g.held_read != held_read {
            epoll_mod(self.ep, c.fd, want_write, !held_read)?;
            g.masked_write = want_write;
            g.held_read = held_read;
        }
        Ok(())
    }

    /// 超时裁决：只裁「治理会话空洞」与「攒不齐的半截帧」，外加把已判死且写干净的连接
    /// 收尾。见 [`GOV_IDLE`] 上方的说明。
    fn sweep(&mut self) {
        let now = Instant::now();
        let mut dead = Vec::new();
        for (fd, c) in self.conns.iter() {
            let buffered = c.buf.lock().unwrap().len();
            let g = c.inner.lock().unwrap();
            if g.dying && g.out.is_empty() {
                dead.push(*fd);
                continue;
            }
            let gov_idle =
                g.role == ConnRole::Gov && !g.busy && now.duration_since(g.last_frame) > GOV_IDLE;
            let stale_partial =
                buffered > 0 && g.q.is_empty() && g.partial.is_some_and(|d| now >= d);
            if gov_idle || stale_partial {
                dead.push(*fd);
            }
        }
        for fd in dead {
            if let Some(c) = self.conns.get(&fd).cloned() {
                self.retire(&c);
            }
        }
    }

    /// 摘掉一条连接：两份表里去掉、订阅登记摘掉，然后靠 `Arc` 关 fd。
    /// 先摘全局表再动别的，是为了让并发的 worker 写出立刻变成「无处可去」，而不是写进一
    /// 条即将关闭的连接。
    fn retire(&mut self, c: &Conn) {
        let fd = c.fd;
        if self.conns.remove(&fd).is_none() {
            return;
        }
        self.shared.conns.lock().unwrap().remove(&fd);
        let sub = c.inner.lock().unwrap().sub.clone();
        if let Some((id, buckets)) = sub {
            super::server_ext::subs_remove(&self.rt, id, &buckets);
        }
        // 没写完的字节不再强求：`SO_LINGER { onoff=1, linger=0 }` 让 close 发 RST 而不是
        // FIN，既不泄漏 fd，也不会让对端把「半截帧 + 优雅关闭」当成一个可用应答
        // （那会伪装成协议错误）。今天往已关 socket 写也会丢这一帧，形状相同。
        c.force_close();
    }
}

impl Conn {
    fn tune(sock: &TcpStream) -> io::Result<()> {
        sock.set_nodelay(true)?;
        sock.set_nonblocking(true)?;
        crate::remote::enable_tcp_keepalive(sock);
        Ok(())
    }

    /// 立即关闭：先挂 `SO_LINGER(on, 0)`，`drop` 时即发 RST。
    fn force_close(&self) {
        let l = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        let _ = unsafe {
            libc::setsockopt(
                self.fd,
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                &l as *const libc::linger as *const libc::c_void,
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            )
        };
    }

    /// 从缓冲区切出尽可能多的完整请求帧。够一帧才消费，不够就把字节留着等下一轮。
    /// `Err` = magic 不对或声明的 body 超过 [`READ_CAP`]，调用方断连。
    fn take_frames(&self, sink: &mut Vec<(u8, Vec<u8>)>) -> io::Result<()> {
        let mut b = self.buf.lock().unwrap();
        let mut cut = 0usize;
        loop {
            let buf = &b[cut..];
            if buf.len() < 9 {
                break;
            }
            if buf[0..4] != crate::remote::proto::MAGIC.to_be_bytes()[..] {
                if cut > 0 {
                    b.drain(..cut);
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "proto: bad magic",
                ));
            }
            let op = buf[4];
            let blen = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]) as usize;
            if blen > READ_CAP {
                if cut > 0 {
                    b.drain(..cut);
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("meta epoll: request body_len {blen} exceeds cap {READ_CAP}"),
                ));
            }
            if buf.len() < 9 + blen {
                break;
            }
            sink.push((op, b[cut + 9..cut + 9 + blen].to_vec()));
            cut += 9 + blen;
        }
        b.drain(..cut);
        drop(b);
        if !sink.is_empty() {
            let mut g = self.inner.lock().unwrap();
            g.last_frame = Instant::now();
            g.partial = None;
        }
        Ok(())
    }
}
