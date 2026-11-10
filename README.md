# lowlat-desk

低延迟远程桌面。控制端可以看见被控 Windows 的屏幕，并用鼠标和键盘操作它。局域网里直接填 IP；跨公网时，双方用同一个设备号和口令，经信令服务器会合。

## 架构

```text
被控端 rd-host                         控制端 rd-client
DXGI Desktop Duplication               收包、按序号重排
        |                                      |
   BGRA -> NV12                          拼回 Annex-B 访问单元
        |                                      |
 H.264（无 B 帧，短 GOP）                  硬件解码，失败则 OpenH264
        |                                      |
 视频：可丢的 UDP 分片                    winit + wgpu 立即呈现
 键鼠：另一条可靠通道  <--------------  鼠标 / 键盘
```

局域网视频走 UDP，键鼠走同一端口上的 TCP。公网先做 Noise 握手，再把这两类数据放进加密的 UDP：视频仍然不重传，键鼠带序号，丢了会重发。

| Crate | 作用 |
| --- | --- |
| `crates/proto` | 传输无关的报文：握手、视频分片、键鼠事件。 |
| `crates/pixel` | BGRA / NV12 转换和缩小。 |
| `crates/codec` | H.264 编码和解码。Windows 走 Media Foundation，macOS 解码走 VideoToolbox，各平台都能退回 OpenH264。 |
| `crates/session` | 口令认证、Noise 加密、可靠键鼠、STUN、码率调节。 |
| `crates/signal` | 信令兼 UDP 中继。跑在一台有公网地址的 Linux 上。 |
| `crates/host` | 被控端。Windows 上抓屏并用 `SendInput` 注入键鼠；其它系统只能用 `--test-pattern`。 |
| `crates/client` | 控制端。Windows 和 macOS 上打开窗口，并把本机按键映射成 Windows 虚拟键。 |

## 报文

默认端口 `47000`。每个 UDP 包前面是 40 字节小端首部（魔数 `RD01`、版本、类型、序号、帧号、分片下标、采集/发送时间戳），后面是负载。视频负载是 Annex-B 的 H.264 访问单元，关键帧重复 SPS/PPS。

控制端发现序号空洞或半帧超时后，发送 `KeyframeRequest`。在下一个 IDR 到来之前，不完整的 P 帧会被丢掉，避免用坏掉的参考帧继续解码。

键鼠不走这条会丢包的视频通道。鼠标坐标是控制端正在显示的那幅画面里的像素；被控端再按采集分辨率和显示器在虚拟桌面上的位置换算，然后调用 `SendInput`。macOS 和 Windows 控制端都按物理键位映射到 Windows 虚拟键。

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

OpenH264 静态链进程序。Windows 上通过 `crt-static` 静态链接 MSVC 运行库，因此预编译包不需要再装 Visual C++。macOS 只依赖系统自带的 VideoToolbox、Metal 和 libc++。

## 下载

每次 CI 成功后，在该次 Actions 运行的 Artifacts 里可以下载这些包（不用自己编译）：

- `lowlat-desk-host-windows-x64`：`rd-host.exe` 和 `使用说明.txt`
- `lowlat-desk-client-windows-x64`：`rd-client.exe` 和 `使用说明.txt`
- `lowlat-desk-client-macos`：通用二进制（arm64 + x86_64）的 `rd-client.app` 和 `使用说明.txt`
- `lowlat-desk-signal-linux-x64`：Linux 上的 `rd-signal` 和 `使用说明.txt`

推送 `v*` 标签时，`.github/workflows/release.yml` 会把同样的四个 zip 挂到 GitHub Release 上。

## 运行

在被控的 Windows 机器上：

```bash
cargo run --release -p rd-host -- --listen 0.0.0.0:47000 --fps 60 --bitrate 8000000 --max-width 1920
```

在控制端（Windows 或 macOS）：

```bash
cargo run --release -p rd-client -- --connect 192.168.1.20:47000
```

窗口会按画面比例缩放到不超过 1600×900，呈现模式是 `AutoNoVsync`。光标在画面范围内移动、点击、滚轮和按键会送到被控端。DXGI 抓到的画面默认不含鼠标光标。

没有 Windows 桌面时，可以用测试图案把编码和 UDP 通路跑起来：

```bash
cargo run -p rd-host -- --test-pattern --listen 127.0.0.1:47000 --width 640 --height 360
```

常用参数：`--fps`、`--bitrate`、`--gop`、`--display`（多显示器时的 DXGI 输出序号）、`--max-width 0`（不缩小）。

## 部署信令服务器

信令程序是 `rd-signal`。它同时监听一个 TCP 端口和一个 UDP 端口，默认 `47010`。被控端用设备号和口令注册，控制端用同一个设备号来找它。口令不会原样交给服务器：服务器只保存一个校验值，双方再用另一个由口令派生的密钥做 Noise 握手（`Noise_NNpsk0`）。视频和键鼠是端到端加密的，中继也只转发密文。

在 VPS 上用 Docker（在仓库根目录构建）：

```bash
docker build -f crates/signal/Dockerfile -t lowlat-signal .
docker run -d --name lowlat-signal --restart unless-stopped \
  -p 47010:47010/tcp -p 47010:47010/udp lowlat-signal
```

或者直接跑二进制：

```bash
cargo build --release -p rd-signal
./target/release/rd-signal --listen 0.0.0.0:47010
```

云厂商安全组和机器防火墙都要放行 **TCP 47010** 和 **UDP 47010**。只开 TCP 时，注册能完成，画面和键鼠过不去。

被控端：

```bash
rd-host.exe --signal 203.0.113.10:47010 --id my-pc --password "一次性口令"
```

控制端：

```bash
rd-client.exe --signal 203.0.113.10:47010 --id my-pc --password "一次性口令"
```

两边先用 STUN 查自己的公网地址，再互相打 UDP 洞。打洞失败，或者加上 `--force-relay` 时，数据改由信令服务器转发。默认 STUN 列表是 `stun.miwifi.com:3478`、`stun.chat.bilibili.com:3478`、`stun.cloudflare.com:3478`、`stun.l.google.com:19302`。Google 的 STUN 在中国大陆经常不通，所以没有把它放在第一位。换成自己的 STUN 用 `--stun stun.example.com:3478`；完全不查 STUN 用 `--stun none`（这时多半会走中继）。

公网链路丢包变多或往返时间超过大约 200 毫秒时，被控端会把码率降到原来的 70%，最低 200 kbps；路径恢复后再慢慢加回去，不超过启动时的 `--bitrate`。

## 要放行的端口

| 位置 | 端口 | 作用 |
| --- | --- | --- |
| 信令 VPS | TCP `47010`、UDP `47010` | 注册、打洞协调、失败时的中继。`--listen` 可以改端口，两个协议都要开。 |
| 被控端（局域网） | TCP `47000`、UDP `47000` | UDP 是画面，TCP 是键鼠。 |
| 被控端（只走公网） | 不需要入站端口 | 它主动连出信令服务器，并对外发 STUN（UDP 3478 或对方的端口）。 |
| 控制端 | 不需要入站端口 | 同样是主动连出。 |

局域网和控制端所在的网络如果挡住了向外的 UDP，打洞会失败，画面会改走信令服务器上的中继。

## 路线图

1. **局域网画面**。UDP 传 H.264。
2. **键鼠**。可靠通道送到被控端，Windows 上用 `SendInput` 注入。
3. **公网**。信令、STUN 打洞、打洞失败时由 `rd-signal` 中继，Noise 加密。

还没做的：声音、文件传输、被控端画面里画出光标、把 DXGI 纹理直接交给硬件编码器。

## 限制

- 局域网直连没有口令。不要把 `47000` 暴露到公网。
- 信令的 TCP 本身不加密。注册校验值会明文经过这条连接。看到它的人可以占用这个设备号，但解不开画面和键鼠。请把信令放在自己的 VPS 上，并使用不容易猜到的设备号和口令。
- 同一时间一个设备号只服务一个控制端。
- 画面静止时不重复编码；对端请求关键帧时才会把上一帧再编成 IDR。
- 软件编码在 1080p、60 帧时可能跟不上，可以把 `--max-width` 降到 1280。
- 打洞依赖两边的 NAT。对称 NAT 通常要走中继。这一仓库里的测试在本机回环上验证了打洞和强制中继，没有在真实的运营商网络上验证。

## CI

`.github/workflows/ci.yml`：

- `ubuntu-latest`：整个工作区的测试（协议、加密会话、本机信令打洞、强制中继、OpenH264 回环），并打包 `rd-signal`。
- `windows-latest`：编译并打包被控端和控制端。
- `macos-latest`：编译并打包控制端。
