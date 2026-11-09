# lowlat-desk

低延迟远程桌面。阶段 1 只做一件事：在局域网里，让控制端**看见**被控端的屏幕。

还没有键鼠控制，也没有跨公网的 NAT 穿透。协议和 crate 边界是按后面两个阶段留出来的。

## 架构

```text
被控端 rd-host                         控制端 rd-client
DXGI Desktop Duplication               UDP 收包、按序号重排
        |                                      |
   BGRA -> NV12                          拼回 Annex-B 访问单元
        |                                      |
 H.264（无 B 帧，短 GOP）                  硬件解码，失败则 OpenH264
        |                                      |
 UDP 分片（约 1200 字节）                  winit + wgpu 立即呈现
```

| Crate | 作用 |
| --- | --- |
| `crates/proto` | 传输无关的报文：握手、视频分片、丢包后请求关键帧。阶段 2 的键鼠报文也定义在这里，但阶段 1 不发送。 |
| `crates/pixel` | BGRA / NV12 转换和缩小。 |
| `crates/codec` | H.264 编码和解码。Windows 走 Media Foundation，macOS 解码走 VideoToolbox，各平台都能退回 OpenH264。 |
| `crates/host` | 被控端。Windows 上抓屏；其它系统只能用 `--test-pattern` 推一帧测试画面。 |
| `crates/client` | 控制端。Windows 和 macOS 上打开窗口显示画面。 |

同一套报文字节以后可以原样放到打洞后的 UDP 套接字或 TURN 中继上。`proto` 不假设局域网广播，也不假设某一种 ICE 实现。

## 报文

默认端口 `47000`。每个 UDP 包前面是 40 字节小端首部（魔数 `RD01`、版本、类型、序号、帧号、分片下标、采集/发送时间戳），后面是负载。视频负载是 Annex-B 的 H.264 访问单元，关键帧重复 SPS/PPS。

控制端发现序号空洞或半帧超时后，发送 `KeyframeRequest`。在下一个 IDR 到来之前，不完整的 P 帧会被丢掉，避免用坏掉的参考帧继续解码。

阶段 2 预留了 `Input` 报文：绝对坐标的鼠标移动、按键、滚轮，以及 Windows `SendInput` 用的虚拟键和扫描码。阶段 1 的两端都忽略它。

## 编码

低延迟设置：

- 不用 B 帧。Media Foundation 上打开 `AVLowLatencyMode`，并把 B 帧数量设为 0；OpenH264 使用 Baseline，本身就没有 B 帧。
- 短 GOP，默认 15 帧一个 IDR。丢包时还会额外强制一个 IDR。
- OpenH264 使用屏幕实时模式、单线程、不跳帧。

选择顺序：

1. **Windows 编码**：枚举 Media Foundation 硬件编码器（驱动注册的 NVENC、Quick Sync 或 AMF），失败则用系统自带的 Microsoft H.264 软件编码器，再失败则用 OpenH264。
2. **Windows 解码**：同样先硬件 MFT，再 Microsoft 软件解码器，再 OpenH264。
3. **macOS 解码**：VideoToolbox。系统会在有硬件解码器时使用它，否则用 Apple 的软件解码器。会话建不起来时退回 OpenH264。
4. **其它平台**：只有 OpenH264。被控端在非 Windows 上不能抓桌面。

阶段 1 会把桌面纹理读回 CPU 再编码。把 DXGI 纹理直接交给硬件编码器可以再省一次回读，留到后面做。

## 延迟日志

两端各自用单调时钟计时，不要求两台机器对时。

- 被控端每秒打一行 `capture->send`：从拿到这一帧到 UDP 发送完成的平均和最大耗时。报文里的 `send_us - capture_us` 是同一段时间。
- 控制端每秒打一行 `receive->present`：从收齐一帧到 `wgpu` 提交呈现的平均和最大耗时（含解码）。同一行会带上报文里的被控端 `capture->send`。

逐帧数字用调试日志：

```bash
RUST_LOG=debug cargo run -p rd-host
RUST_LOG=debug cargo run -p rd-client -- --connect 192.168.1.20:47000
```

## 构建

需要 Rust 1.88 或更新（CI 使用 stable）。OpenH264 由 `openh264` crate 自带的源码编译，因此需要 C++ 工具链：Windows 用 MSVC，macOS 用 Clang，Linux 上需要 `g++`。

```bash
cargo build --release -p rd-host -p rd-client
cargo test --workspace
```

非 Windows 环境会正常编过被控端；真正的 DXGI 抓屏代码用 `cfg(windows)` 隔开。

## 运行

在被控的 Windows 机器上：

```bash
cargo run --release -p rd-host -- --listen 0.0.0.0:47000 --fps 60 --bitrate 8000000 --max-width 1920
```

在控制端（Windows 或 macOS）：

```bash
cargo run --release -p rd-client -- --connect 192.168.1.20:47000
```

窗口会按画面比例缩放到不超过 1600×900，呈现模式是 `AutoNoVsync`。DXGI 抓到的画面默认不含鼠标光标。

没有 Windows 桌面时，可以用测试图案把编码和 UDP 通路跑起来：

```bash
cargo run -p rd-host -- --test-pattern --listen 127.0.0.1:47000 --width 640 --height 360
```

常用参数：`--fps`、`--bitrate`、`--gop`、`--display`（多显示器时的 DXGI 输出序号）、`--max-width 0`（不缩小）。

## 路线图

1. **局域网画面（本阶段）**。UDP 传 H.264，控制端只看画面。
2. **键鼠**。控制端发送已经定义好的 `Input` 报文，被控端用 `SendInput` 注入。
3. **公网**。信令服务器交换地址，STUN 打洞，打洞失败时走 TURN。报文格式保持不变。

## 限制

- 没有认证。只适合可信局域网。
- 同一时间只服务最后一次握手的那个控制端。
- 画面静止时不重复编码；对端请求关键帧时才会把上一帧再编成 IDR。
- 阶段 1 的软件编码在 1080p、60 帧时可能跟不上，可以把 `--max-width` 降到 1280。

## CI

`.github/workflows/ci.yml`：

- `ubuntu-latest`：整个工作区的测试（协议、色彩转换、OpenH264 回环、丢包后请求关键帧）。
- `windows-latest`：编译被控端，并编译控制端。
- `macos-latest`：编译控制端。
