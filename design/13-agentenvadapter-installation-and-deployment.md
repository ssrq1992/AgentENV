# SSF AgentENV 适配：端到端安装部署与验收手册

更新日期：2026-10-19。适用对象：当前仓库 `SSF/adapter/agentenvadapter`、AgentENV v0.2.3 加外部设备补丁。

本文给出从准备 Linux Worker、构建镜像、安装共享账本和 CSI、签发证书、构建模板，到创建沙箱、guest 读写、释放和恢复的完整操作顺序。**以下集群、Linux 挂载、Rust 编译和真实 VM 步骤是待执行的部署流程，尚未在当前 macOS 主机实跑；不能将本文当作端到端通过报告。** 已有验证记录见 [适配验证摘要](../SSF/adapter/agentenvadapter/validation/summary.json)。

主线采用一个真实 Linux/KVM Worker、一个隔离 Kubernetes 测试集群和仓库自带块 CSI Driver，以便把所有依赖部署具体化。已有企业集群可从环境检查开始；本文不代替集群发行版的控制平面、CNI 或高可用安装手册。厂商 Driver、多 Worker、生产数据库和 PKI 的替换点在第 16 节列出。

## 目录与执行顺序

1. [部署范围与最终形态](#1-部署范围与最终形态)
2. [环境与版本](#2-环境与版本)
3. [目录、变量与源码检查](#3-目录变量与源码检查)
4. [构建、发布镜像](#4-构建发布镜像)
5. [配置与分阶段生成清单](#5-配置与分阶段生成清单)
6. [证书、签名密钥与数据库凭据](#6-证书签名密钥与数据库凭据)
7. [安装数据库、RBAC 与迁移](#7-安装数据库rbac-与迁移)
8. [安装 Guard 与准入保护](#8-安装-guard-与准入保护)
9. [安装测试 CSI 与 Gateway](#9-安装测试-csi-与-gateway)
10. [构建兼容外部卷的模板](#10-构建兼容外部卷的模板)
11. [启动节点和控制面](#11-启动节点和控制面)
12. [冷启动、读写、删除与复用](#12-冷启动读写删除与复用)
13. [WFFC、静态 PV 与只读验收](#13-wffc静态-pv-与只读验收)
14. [自动化验收与性能报告](#14-自动化验收与性能报告)
15. [诊断、升级、恢复与卸载](#15-诊断升级恢复与卸载)
16. [生产替换与多节点部署](#16-生产替换与多节点部署)
17. [交付检查与执行记录](#17-交付检查与执行记录)

## 1. 部署范围与最终形态

### 1.1 本版本支持什么

支持通过独立适配 API 创建卷、创建/查询/删除沙箱、查看操作，以及代理 guest 的进程和文件操作。卷为标准 CSI `volumeMode: Block`，guest 使用 ext4；覆盖冷启动和兼容模板启动。静态供给、动态 Immediate、动态/静态 WFFC 均属于实现范围。

以下能力不在当前 AgentENV 适配范围：FileTree、带外部卷的暂停/恢复、快照、fork、在线扩容和自动恢复 VM 内存。SSF 公共框架有相应管理接口，不表示此适配器已经开放。AgentENV 原有 OverlayBD/ublk 根盘和旧卷仍走原有路径。

创建一个沙箱不会创建一个 Kubernetes Pod。Kubernetes 负责后端卷生命周期；适配控制面内嵌 Consumption，直接 Attach；SSF Node 直接 Stage/Publish；AgentENV 将设备接入 Firecracker 并核验 guest。

### 1.2 需要部署的对象

![AgentENV 适配部署](figures/agentenvadapter-deployment.svg)

[可编辑 Mermaid](figures/agentenvadapter-deployment.mmd) · [PNG](figures/agentenvadapter-deployment.png) · [完整设计与时序](12-agentenvadapter-design-and-implementation.md)

图中的 K 是原生 Kubernetes/CSI 配套组件，N 是 SSF 公共实现，A 是适配工程，P 是 AgentENV 扩展，D 是存储 Driver，B 是后端。本文的本地块测试 Driver 将 Controller/Node 放在同一 Pod；厂商通常拆成 Controller Deployment 与 Node DaemonSet。

| Namespace / 对象 | Pod 内容器或进程 | 职责 |
| --- | --- | --- |
| `ssf-system/ssf-postgres`，测试 Deployment ×1 | PostgreSQL | SSF 消费账本和独立命名的适配元数据表；主线使用持久 HostPath |
| `ssf-system/ssf-guard`，Deployment ×2 | `ssf-guard` | 受管 PVC/PV 和原生 Pod 消费冲突保护 |
| `ssf-fixture/ssf-block`，单 Worker DaemonSet | test Driver、registrar、provisioner、resizer、Gateway | 原生供卷/注册，及经过账本授权的直接 Attach/Detach |
| `agentenv-ssf/agentenv-control`，Deployment ×2 | `agentenvadapter control`，内含 Consumption | mTLS 身份、计算预留、PVC 解析、消费与恢复 |
| `agentenv-ssf/aenv-<node>`，每 Worker 一个 DaemonSet | AgentENV + `agentenvadapter node` | 同一 Pod 的共享 PID namespace；VMM、guest 和生命周期证据 |
| `agentenv-ssf/ssf-aenv-<node>`，每 Worker 一个 DaemonSet | `ssf-node` | 独立 Pod；CSI Node 调用和持久 WAL |
| `agentenv-ssf/agentenv-migrate`，Job | `agentenvadapter ctl ... migrate` | 部署前迁移两组 SQL 表，成功后退出 |
| 临时 `agentenv-template-builder` Pod | 已打补丁的 AgentENV | 开放消费入口前构建无外部卷模板，结束后删除 |

没有强制性的 `ssf-controller` Pod，也不需要为这条路径部署 AgentENV Gateway/Scheduler。用户只访问适配控制面；Node 服务固定指向选中的 Worker，不能经另一个 Gateway 再次调度。

### 1.3 端口和持久目录

| 入口 | 端口 | 访问边界 |
| --- | --- | --- |
| 适配控制面 HTTPS API | 9444 | 租户证书 mTLS；URI 映射 tenant + namespace |
| 节点适配 HTTPS API / Evidence | 9444 | 仅 control 和对应 SSF Node 身份；NetworkPolicy 再限制来源 |
| SSF Node gRPC | 9443 | 对应节点适配器 mTLS 身份 |
| CSI Gateway gRPC | 9443 | control mTLS 身份；原生 CSI UDS 在 Pod 内 |
| Guard Service | 443 → 9443 | API Server 校验服务证书；webhook 不要求客户端证书 |
| AgentENV 原生 API | `127.0.0.1:8000` | 仅所在 Pod，另需原生 API key |
| 外部设备 Hook | `127.0.0.1:9445` | 仅所在 Pod |
| 健康/指标 | 适配 9091；SSF Node/Gateway 9090 | 内部运维网络；不携带业务认证信息 |
| PostgreSQL | 5432 | control、迁移、Guard、Gateway |

| Worker 路径 | 内容 | 消费 Pod 中的路径/要求 |
| --- | --- | --- |
| `/var/lib/ssf` | Stage/Publish 路径、SSF WAL | 同路径；CSI/SSF 为 Bidirectional，AgentENV/适配器为 HostToContainer |
| `/var/lib/agentenvadapter` | 节点适配 bbolt | 同路径；单个节点进程独占 |
| `/var/lib/agentenv-external` | AgentENV 外部设备生命周期日志 | 原生进程与适配器共同可见 |
| `/var/lib/aenv` | 原生运行数据、OverlayBD/ublk 缓存和依赖 | `/workspace` |
| `/var/lib/aenv-templates` | 本文单节点模板仓库 | `/templates`；构建和消费路径一致 |
| `/var/lib/aenv-builder` | 独立模板构建工作区 | 构建 Pod 的 `/workspace` |
| `/var/lib/ssf-fixture/block` | 测试 Driver 的后端文件 | Driver `/data`；只代表本节点存储 |
| `/var/lib/ssf-postgres` | 测试数据库持久数据 | PostgreSQL 数据目录 |

不要删除这些目录来解除 Pending、writer 冲突或 bbolt 锁。它们共同参与恢复。业务 I/O 从 guest 经虚拟块设备到宿主设备和存储后端，不经过 control、Guard 或 Gateway 的管理 RPC。

## 2. 环境与版本

### 2.1 主线环境

- 已安装 Kubernetes、DNS 和支持 NetworkPolicy 的 CNI。本文按仓库的 Kubernetes 1.35.0 测试基线编写，不代表其他版本已经认证。管理端 kubectl 与集群使用兼容版本。
- 一台专用 Linux amd64 Worker，建议测试起点 8 vCPU / 16 GiB；示例只预留其中 4 vCPU / 4 GiB 给沙箱，剩余供 AgentENV、CSI、数据库和系统使用。磁盘要容纳镜像、模板、数据库及测试卷；先以 100 GiB 可用空间作为实验预算，再按实际镜像大小调整。
- 宿主 Ubuntu 24.04 / Linux 6.8+，实际可用的 `/dev/kvm`、`/dev/ublk-control`、`/dev/loop-control`、`/dev/net/tun`。虚拟化宿主需要启用并验证嵌套虚拟化。
- 容器运行时支持 privileged、HostPath、共享 PID namespace 和双向挂载传播。业务节点不能同时接受其他绕过适配器的 AgentENV 创建请求。
- 构建机有 Docker BuildKit、Git、Python 3、OpenSSL、curl、jq；直接跑 Go/Rust 检查时还需对应编译器。管理机可以是 macOS，真实 VM 必须在 Linux Worker 上执行。

`shareProcessNamespace: true` 让同 Pod 容器看到彼此的进程，不能代替 hostPID，也不能证明另一个 Pod 的旧进程已经停止。[Kubernetes 共享进程命名空间说明](https://kubernetes.io/docs/tasks/configure-pod-container/share-process-namespace/)

挂载传播必须在真实宿主生效；双向传播要求特权容器，错误配置可能表现为 SSF 看见 Publish，而 VMM 看不到设备。以 [Kubernetes 挂载传播说明](https://kubernetes.io/docs/concepts/storage/volumes/#mount-propagation) 和本文真实设备验收共同检查。

**kind 只用于 API/资源集成测试。默认 kind 配置不是已经配置好 KVM、ublk、loop、PID 和挂载传播的真实 VM 环境。** 不使用 Docker Desktop/macOS 结果替代 Worker 验收。

### 2.2 当前构建输入

| 项目 | 仓库当前值 | 权威文件 |
| --- | --- | --- |
| AgentENV | v0.2.3，基线 `6cccaa7842bd5be2051111d4f74d9e37aa721244` + 适配补丁 | [补丁清单](../SSF/adapter/agentenvadapter/patches/manifest.json) |
| Go 工具链 | 适配镜像 1.26.0；SSF/测试镜像 1.26.3 | 各自 Dockerfile；不能将它们误记为同一镜像版本 |
| Rust / cargo-chef | 1.99.1 / 0.1.71 | [构建脚本](../SSF/adapter/agentenvadapter/scripts/build-agentenv.sh) |
| PostgreSQL | `postgres:17.4` | 本文测试清单 |
| CSI registrar / provisioner / resizer | v2.13.0 / v5.2.0 / v1.13.2 | [fixture 渲染器](../SSF/scripts/render_fixture.py) |
| Firecracker / guest kernel | 1.15.1-patch-v1 / vmlinux-6.1.175 | [AgentENV 依赖清单](../AgentENV/config/deps_manifest.toml) |
| OverlayBD | v1.0.18-aenv.1 | 同上 |

上述是源码声明的构建输入，不是本次已经成功拉取、编译和运行的版本认证。镜像 tag、apt 源仍可能变化；正式复现报告必须补充拉取到的 digest、构建日志及使用的 Cargo.lock/go.sum。宿主内核和 guest 内核是两件事。

### 2.3 在 Worker 上检查

以下命令在 **Linux Worker 的宿主 shell** 执行，不在 macOS 或普通容器内执行：

```bash
uname -m
uname -r
sudo modprobe kvm
sudo modprobe ublk_drv
sudo modprobe loop
ls -l /dev/kvm /dev/ublk-control /dev/loop-control /dev/net/tun
cat /proc/sys/kernel/random/boot_id
findmnt -o TARGET,PROPAGATION / /var/lib/kubelet
df -h /var/lib
```

预期 amd64/x86_64、设备实际存在且容器运行身份可用。若 `ublk_drv` 缺失，先由平台管理员安装与当前内核匹配的模块；AgentENV 的 [宿主准备脚本](../AgentENV/scripts/docker-setup.sh) 可作参考，它还会改内核参数，执行前阅读实际内容。不通过卸载正在使用的 ublk 模块修复问题。

检查 `/proc/<pid>/limits` 的 locked memory、PID 上限、文件描述符上限，以及 Worker 网络/TUN、iptables。这些取决于容器运行时和发行版；真实启动失败时查看原生 AgentENV 日志。不能仅凭设备文件存在认定 Firecracker 可运行。

## 3. 目录、变量与源码检查

除特别标注外，后续命令在管理/构建机的 **Bash** 中执行，工作目录始终为 `sandboxVolume/SSF`。示例固定 Namespace 为 `agentenv-ssf`、`ssf-system`、`ssf-fixture`、`agentenv-demo`；这与证书 DNS、RBAC 和配置一致。

```bash
cd /path/to/sandboxVolume/SSF
export SSF_ROOT="$PWD"
export ADAPTER="$SSF_ROOT/adapter/agentenvadapter"
export CTX='REPLACE_WITH_TEST_CONTEXT'
export WORKER='REPLACE_WITH_KUBERNETES_NODE_NAME'
export REGISTRY='REPLACE_WITH_REGISTRY/ssf-lab'
export AGENT_IMAGE="$REGISTRY/agentenv-ssf:0.2.3"
export ADAPTER_IMAGE="$REGISTRY/agentenvadapter:0.1.0"
export SSF_IMAGE="$REGISTRY/ssf:0.1.0"
export TEST_IMAGE="$REGISTRY/ssf-test:0.1.0"
export GUEST_IMAGE='docker.io/library/ubuntu:24.04'
# 正式验收将 GUEST_IMAGE 换为实际可拉取的不可变 @sha256 引用。
umask 077
export RUN
RUN=$(mktemp -d "${TMPDIR:-/tmp}/ssf-agentenv-install.XXXXXX")
mkdir -p "$RUN/reports"
k() { kubectl --context "$CTX" "$@"; }
k cluster-info
k get node "$WORKER" -o wide
k get node "$WORKER" -o json > "$RUN/reports/worker-before.json"
k version -o json > "$RUN/reports/kubernetes-version.json"
git -C ../AgentENV rev-parse HEAD
git -C ../AgentENV status --short
```

先替换三个 `REPLACE_WITH...`。所有写集群命令使用显式 context，不切换或依赖全局默认 context。`RUN` 含私钥和测试数据库口令，权限为当前用户独占；备份时将秘密与可公开的 reports 分开。

当前 AgentENV 工作树已经包含补丁，**不要再次 apply**。在另一份干净 v0.2.3 源码安装时，先比对基线，再执行 `git apply --check` 和 `git apply`；补丁及许可证在 [patches](../SSF/adapter/agentenvadapter/patches/manifest.json)。不要用 hard reset 覆盖已有修改。

可以只读校验当前七个修改文件：

```bash
python3 - <<'PY'
import hashlib, json, pathlib
root = pathlib.Path('../AgentENV')
m = json.loads(pathlib.Path('adapter/agentenvadapter/patches/manifest.json').read_text())
for name, expected in m['files'].items():
    actual = hashlib.sha256((root / name).read_bytes()).hexdigest()
    assert actual == expected, f'source differs from delivered patch: {name}'
print('AgentENV patch file hashes match')
PY
```

## 4. 构建、发布镜像

### 4.1 构建四个镜像

在 Linux amd64 构建机执行；跨架构构建需自行配置 buildx，不能拿 macOS 编译产物直接作为 Linux 可运行程序。

```bash
set -o pipefail
docker build -f Dockerfile -t ssf:0.1.0 . \
  2>&1 | tee "$RUN/reports/build-ssf.log"
make -C adapter/agentenvadapter image \
  2>&1 | tee "$RUN/reports/build-adapter.log"
bash adapter/agentenvadapter/scripts/build-agentenv.sh \
  2>&1 | tee "$RUN/reports/build-agentenv.log"
docker build -f testing/Dockerfile -t ssf-test:0.1.0 . \
  2>&1 | tee "$RUN/reports/build-fixture.log"
```

AgentENV 构建脚本临时生成固定 Rust/cargo-chef 版本的 Dockerfile，沿用原 Cargo.lock 和依赖清单，不修改原 Dockerfile。构建期间需要访问依赖下载源和 OCI 仓库。Rust 构建或依赖下载失败时停止，不继续部署一个同 tag 的旧镜像。

```bash
docker tag ssf:0.1.0 "$SSF_IMAGE"
docker tag agentenvadapter:0.1.0 "$ADAPTER_IMAGE"
docker tag agentenv-ssf:0.2.3 "$AGENT_IMAGE"
docker tag ssf-test:0.1.0 "$TEST_IMAGE"
docker push "$SSF_IMAGE"
docker push "$ADAPTER_IMAGE"
docker push "$AGENT_IMAGE"
docker push "$TEST_IMAGE"
docker image inspect "$SSF_IMAGE" "$ADAPTER_IMAGE" "$AGENT_IMAGE" "$TEST_IMAGE" \
  > "$RUN/reports/build-images.json"
```

Worker 必须能拉取这些镜像和 CSI sidecar/PostgreSQL 镜像。私有仓库的 `imagePullSecrets` 要配置到各 Namespace 的对应 ServiceAccount/Pod；模板构建 Pod 使用 default SA，不能遗漏。离线部署需将所有镜像导入 **实际 CRI 使用的存储**，仅 `docker load` 到另一个 Docker daemon 不足以供 containerd kubelet 使用。

为了复现，可在后续渲染前将四个 `*_IMAGE` 变量改为构建后记录的 `registry/...@sha256:...`。

### 4.2 可单独运行的源码检查

```bash
go test ./...
go vet ./...
go test -race ./...
(cd ../AgentENV && cargo check --locked -p agentenv)
(cd ../AgentENV && cargo test --locked -p agentenv --lib)
```

PostgreSQL/集群测试没有环境变量时会 Skip；检查输出，不将 `go test` 的零退出码理解为所有集成项都执行。Linux/真实 VM 检查见第 14 节。

## 5. 配置与分阶段生成清单

### 5.1 控制配置

```bash
python3 - <<'PY'
import json, os, pathlib
c = json.loads(pathlib.Path(os.environ['ADAPTER'], 'config/control.example.json').read_text())
c['workers'] = [{'name': os.environ['WORKER'],
                 'url': 'https://placeholder.invalid:9444',
                 'capacity': {'cpu': 4, 'memory_mib': 4096}}]
c['drivers'] = [{'Name': 'test.block.ssf.dev',
                 'ControllerEndpoint': 'ssf-gateway.ssf-fixture.svc:9443', 'Revision': '1'}]
c['templates'] = {}
pathlib.Path(os.environ['RUN'], 'control.json').write_text(json.dumps(c, indent=2))
PY
```

渲染器会将 Worker URL 改成该节点固定 Service；`WORKER` 必须是 Kubernetes Node 名称，不能用 IP、CSI node_id 或随意别名。Node UID/Boot ID 在运行时读取，不写死到此配置。CSI node_id 来自 CSINode/Driver，不等于节点名字。

`capacity` 表示此适配器可预留的沙箱容量，不是整台机器总量；AgentENV Pod 的 requests 也使用这组值。预留之外要留系统余量。`principals` 的 demo 身份只授权 `agentenv-demo`；增加身份时同步签发正确 URI SAN，同 tenant 的 namespace 集合必须一致。

### 5.2 渲染辅助脚本

本文提供 [安装清单渲染器](../SSF/adapter/agentenvadapter/deploy/installation/render.py)。它只读配置并生成 JSON，不连接集群、不创建证书、不操作磁盘设备。它限制为 **一个 Worker**，避免把单节点 loop 后端误部署成跨节点共享存储。

该脚本复用原适配渲染器、SSF base 及 fixture，然后加上本文安装覆盖配置：

- 去掉通用 `ssf-node` DaemonSet，只保留本适配每节点专属 SSF Node。
- SSF Node 显式 `runAsUser: 0`。公共镜像默认 UID 65532，仅设置 privileged 不能代替运行身份配置。
- AgentENV 增加初始化容器，将镜像内预置 `/workspace` 内容以 `cp -an` 方式补到 HostPath，不覆盖已有数据；否则空 HostPath 会遮住镜像依赖。
- 配置单节点模板仓库和临时构建 Pod。
- 节点/fixture DaemonSet 使用 `OnDelete`，避免改配置时自动替换运行中的 VMM Pod。它不免除管理员先排空的责任。
- 增加测试 PostgreSQL，以及和测试 Driver 共享 UDS 的 Gateway。所有测试简化都在生成文件名中标明。

```bash
k create --dry-run=client --validate=false -f deploy/base.yaml -o json > "$RUN/base.json"
render_install() {
  python3 "$ADAPTER/deploy/installation/render.py" \
    --control "$RUN/control.json" --base-json "$RUN/base.json" --out "$RUN/manifests" \
    --agent-image "$AGENT_IMAGE" --adapter-image "$ADAPTER_IMAGE" \
    --ssf-image "$SSF_IMAGE" --test-image "$TEST_IMAGE"
}
render_install
```

| 生成文件 | 应用时机 |
| --- | --- |
| `00-namespaces.json` | 最先；测试 Namespace 包含允许 privileged/HostPath 的 PSA 标签 |
| `01-rbac.json` | Namespace 后，另应用公共 `consumption-rbac.yaml` |
| `02-postgres-test-only.json` | 数据库 Secret 后 |
| `03-app-config.json` | 配置、Service、NetworkPolicy；不会启动 VM |
| `04-migration.json` | DB 就绪后，等待 Job Complete |
| `05-guard.json` | 迁移后，webhook 之前 |
| `06-block-fixture-test-only.json` | Gateway 证书和账本就绪后 |
| `07-template-builder.json` | 正式 Worker Pod 尚未创建时 |
| `08-workers.json` | 模板构建完成且构建 Pod 已停止后 |
| `09-control.json` | Node、CSI、Guard 都可用后 |

**不要 `kubectl apply -f "$RUN/manifests"` 一次全量应用。** 以下章节逐步应用并验证。

## 6. 证书、签名密钥与数据库凭据

### 6.1 适配器测试 PKI

```bash
python3 "$ADAPTER/scripts/dev-certs.py" \
  --control "$RUN/control.json" --out "$RUN/pki"
```

脚本创建新的目录，重复执行不会覆盖已有密钥。测试 CA 和证书仅 7 天有效；正式部署用企业 PKI，并给每个服务证书配置唯一 URI SAN。不要将 `pki` 提交源码仓库。

| Secret | URI SAN / 用途 |
| --- | --- |
| `agentenv-control-tls` | `spiffe://ssf/agentenv/control` |
| `aenv-<slug>-tls` | `spiffe://ssf/agentenv/node/<NodeName>` |
| `ssf-aenv-<slug>-tls` | `spiffe://ssf/agentenv/ssf-node/<NodeName>` |
| 本地 `user.crt/user.key` | `spiffe://ssf/agentenv/user/demo`；不放进服务 Pod |
| `agentenv-plan-private` | control 的 Ed25519 签名密钥 |
| `agentenv-plan-public` | node/SSF Node 的验签公钥 |
| `agentenv-native-key` | AgentENV 原生 API key，仅原生进程和节点适配器持有 |

`slug` 为 Node 名转小写后将非 `[a-z0-9-]` 替换为 `-`。DNS SAN 必须匹配实际 `service.namespace.svc`。测试证书另外带 localhost 和 127.0.0.1 SAN，便于本机 port-forward。

计划密钥文件内容是 **base64 编码的原始 Ed25519 key**：private 64 字节、public 32 字节；不是 PEM。Kubernetes Secret 的 `data` 又有一层 base64，手工配置时勿混淆。

### 6.2 补齐 Guard 和 Gateway 证书

它们不由 `dev-certs.py` 生成。以下用同一测试 CA 签发；Guard 是服务端 TLS，Gateway 是 mTLS 服务端：

```bash
issue_test_cert() {
  local name="$1" dns="$2" uri="$3"
  printf 'subjectAltName=DNS:%s,URI:%s\nextendedKeyUsage=serverAuth,clientAuth\n' \
    "$dns" "$uri" > "$RUN/pki/$name.ext"
  openssl req -newkey rsa:2048 -nodes -subj "/CN=$name" \
    -keyout "$RUN/pki/$name.key" -out "$RUN/pki/$name.csr"
  openssl x509 -req -in "$RUN/pki/$name.csr" \
    -CA "$RUN/pki/ca.crt" -CAkey "$RUN/pki/ca.key" \
    -set_serial "0x$(openssl rand -hex 16)" -days 7 \
    -extfile "$RUN/pki/$name.ext" -out "$RUN/pki/$name.crt"
}
issue_test_cert ssf-guard ssf-guard.ssf-system.svc spiffe://ssf/guard
issue_test_cert ssf-gateway ssf-gateway.ssf-fixture.svc spiffe://ssf/gateway
openssl verify -CAfile "$RUN/pki/ca.crt" "$RUN/pki/ssf-guard.crt" "$RUN/pki/ssf-gateway.crt"
```

### 6.3 生成测试数据库凭据

此主线为了独立复现，使用单机 PostgreSQL、内部无 TLS 连接和同一个测试 owner。**这是测试配置，不是生产账号/容灾设计。** 生产应使用第 16 节的账号拆分、TLS、备份及外部持久数据库。

```bash
python3 - <<'PY'
import base64, json, os, pathlib, secrets
r = pathlib.Path(os.environ['RUN'])
password = secrets.token_hex(32)
dsn = f'postgres://postgres:{password}@ssf-postgres.ssf-system.svc:5432/ssf?sslmode=disable'
items = []
for ns, name, key, value in [
    ('ssf-system', 'ssf-postgres', 'password', password),
    ('ssf-system', 'ssf-ledger', 'url', dsn),
    ('ssf-fixture', 'ssf-ledger', 'url', dsn),
    ('agentenv-ssf', 'agentenv-ledger', 'url', dsn),
]:
    items.append({'apiVersion': 'v1', 'kind': 'Secret', 'type': 'Opaque',
                  'metadata': {'name': name, 'namespace': ns},
                  'data': {key: base64.b64encode(value.encode()).decode()}})
(r/'database-secrets.json').write_text(json.dumps({'apiVersion': 'v1', 'kind': 'List', 'items': items}, indent=2))
PY
```

初次安装生成一次即可。重装复用原数据目录时必须保留原口令；更改 `POSTGRES_PASSWORD` Secret 不会自动修改已初始化数据库内的账号密码。

## 7. 安装数据库、RBAC 与迁移

### 7.1 Namespace、Secret 和权限

```bash
k apply -f "$RUN/manifests/00-namespaces.json"
k apply -f "$RUN/pki/secrets.json"
k apply -f "$RUN/database-secrets.json"
k -n ssf-system create secret generic ssf-guard-tls \
  --from-file=tls.crt="$RUN/pki/ssf-guard.crt" \
  --from-file=tls.key="$RUN/pki/ssf-guard.key" --from-file=ca.crt="$RUN/pki/ca.crt" \
  --dry-run=client -o yaml | k apply -f -
k -n ssf-fixture create secret generic ssf-gateway-tls \
  --from-file=tls.crt="$RUN/pki/ssf-gateway.crt" \
  --from-file=tls.key="$RUN/pki/ssf-gateway.key" --from-file=ca.crt="$RUN/pki/ca.crt" \
  --dry-run=client -o yaml | k apply -f -
k apply -f deploy/consumption-rbac.yaml
k apply -f "$RUN/manifests/01-rbac.json"
k apply --dry-run=server -f "$RUN/manifests/03-app-config.json"
k apply -f "$RUN/manifests/03-app-config.json"
```

秘密经 stdin 管道传给 kubectl，不在屏幕输出。命令成功不意味着 tenant 有 Kubernetes RBAC；tenant 的使用权限由 control 的 mTLS principal map 决定，服务自己使用 ServiceAccount 访问 Kubernetes。

### 7.2 启动数据库并迁移

```bash
k apply --dry-run=server -f "$RUN/manifests/02-postgres-test-only.json"
k apply -f "$RUN/manifests/02-postgres-test-only.json"
k -n ssf-system rollout status deployment/ssf-postgres --timeout=300s
k apply --dry-run=server -f "$RUN/manifests/04-migration.json"
k apply -f "$RUN/manifests/04-migration.json"
k -n agentenv-ssf wait --for=condition=complete job/agentenv-migrate --timeout=300s
k -n agentenv-ssf logs job/agentenv-migrate > "$RUN/reports/migration.log"
k -n ssf-system exec deployment/ssf-postgres -- \
  psql -U postgres -d ssf -c '\dt' > "$RUN/reports/database-tables.txt"
```

应同时看到 `ssf_volumes/operations/bindings/attachments/...` 和 `agentenv_adapter_operations/reservations/volumes`。适配表只存项目操作、实例和预留；卷消费排他仍唯一在 SSF 表。

迁移失败时检查 Job 日志、数据库 DNS/口令/权限/版本，不跳过 Job 启动 control。升级时 Job 名和 Pod 模板不可原地任意改：确认原 Job 已结束后，备份数据库，再删除 **该已结束 Job 对象** 并重新应用新 Job；不要删除 PostgreSQL 数据目录。

## 8. 安装 Guard 与准入保护

### 8.1 先服务，后 webhook

```bash
k apply --dry-run=server -f "$RUN/manifests/05-guard.json"
k apply -f "$RUN/manifests/05-guard.json"
k -n ssf-system rollout status deployment/ssf-guard --timeout=180s
python3 - <<'PY'
import base64, os, pathlib
r = pathlib.Path(os.environ['RUN'])
s = pathlib.Path('deploy/webhook.yaml').read_text()
s = s.replace('CA_BUNDLE', base64.b64encode((r/'pki/ca.crt').read_bytes()).decode())
(r/'webhook.yaml').write_text(s)
PY
k apply --dry-run=server -f "$RUN/webhook.yaml"
k apply -f "$RUN/webhook.yaml"
k get validatingwebhookconfiguration ssf-guard -o yaml > "$RUN/reports/webhook.yaml"
```

`failurePolicy: Fail` 会让匹配的请求在 Guard 不可用时被拒绝。当前 webhook 匹配整个集群的 PVC/PV 和 Pod 操作，因此主线要求专用测试集群；生产的可用性和影响面需要平台审查。不能简单加一个错误的 namespaceSelector 后假设集群级 PV 仍被完整保护。

Guard readiness 只证明其数据库连接可用；还必须执行下面的实际 Admission 检查。

### 8.2 准入烟雾测试

先创建一个没有供卷器的静态测试类及 Pending 受管 PVC，不会创建后端卷。后续由适配 API 排空删除它。

```bash
cat > "$RUN/guard-probe.yaml" <<'YAML'
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata: {name: ssf-probe-static}
provisioner: kubernetes.io/no-provisioner
reclaimPolicy: Retain
volumeBindingMode: WaitForFirstConsumer
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: guard-probe
  namespace: agentenv-demo
  labels: {ssf.dev/managed: 'true'}
spec:
  storageClassName: ssf-probe-static
  volumeMode: Block
  accessModes: [ReadWriteOnce]
  resources: {requests: {storage: 1Gi}}
YAML
k apply -f "$RUN/guard-probe.yaml"
cat > "$RUN/guard-conflict-pod.yaml" <<'YAML'
apiVersion: v1
kind: Pod
metadata: {name: must-be-rejected, namespace: agentenv-demo}
spec:
  containers:
    - name: probe
      image: busybox:1.37.0
      command: [sleep, '3600']
      volumeDevices: [{name: data, devicePath: /dev/test-volume}]
  volumes:
    - name: data
      persistentVolumeClaim: {claimName: guard-probe}
YAML
if k create --dry-run=server -f "$RUN/guard-conflict-pod.yaml"; then
  printf '%s\n' 'FAIL: Guard did not reject native Pod consumption'
else
  printf '%s\n' 'Inspect error: it must identify the SSF Guard denial, not an unrelated RBAC/network error'
fi
k delete pvc guard-probe -n agentenv-demo --dry-run=server
```

预期最后两种请求都被 Guard 拒绝：原生 Pod 不可消费受管卷、PVC 没有排空许可不能删除。仅命令报错不够，需核对拒绝原因并保存日志。若 Guard 意外允许，先停止继续部署并查明保护缺口。

## 9. 安装测试 CSI 与 Gateway

```bash
k apply --dry-run=server -f "$RUN/manifests/06-block-fixture-test-only.json"
k apply -f "$RUN/manifests/06-block-fixture-test-only.json"
k -n ssf-fixture rollout status daemonset/ssf-block --timeout=300s
k -n ssf-fixture get pods -l app=ssf-block -o wide
k get csidriver test.block.ssf.dev -o yaml
k get csinode "$WORKER" -o json > "$RUN/reports/csinode.json"
k get sc ssf-block-immediate ssf-block-wffc -o yaml > "$RUN/reports/storageclasses.yaml"
k -n ssf-fixture logs daemonset/ssf-block -c gateway --tail=60
```

应只有该 Worker 上一个 fixture Pod，CSINode 有 `test.block.ssf.dev` 的真实 nodeID，两个 StorageClass 都为 `Retain`，分别是 `Immediate` 和 `WaitForFirstConsumer`。节点注册和 sidecar 日志正常后才继续。

Gateway 与 Controller 共用 `/csi/csi.sock`，监听 `ssf-gateway.ssf-fixture.svc:9443`，只允许 control 的 URI 身份。Gateway 读取同一消费账本；它不会创建/删除卷，只代理 Attach/Detach 和必要只读 CSI 调用。

这个测试 Driver 使用本地后端文件和 loop 设备，Controller 与 Node 在一个节点，测试 RBAC 使用 cluster-admin。**它用于隔离环境验证接口与真实块设备流程，不代表厂商远端卷、多节点故障转移或生产权限设计。** 不增加多个 fixture Worker 后通过同一个 Service 随机访问其本地 Controller。

厂商安装路径见第 16 节。使用厂商 Driver 时跳过本节 fixture 清单，保留标准 PVC/PV 生命周期组件、节点注册和适配所需 Gateway/直接 TLS endpoint。

## 10. 构建兼容外部卷的模板

### 10.1 构建环境与时机

主线为单 Worker 实验，先运行独立模板构建 Pod，**此时 `08-workers` 和 `09-control` 尚未部署，节点尚未开放给适配消费**。构建结束并确认退出后才启动消费服务。这样不会在适配容量预留之外并发创建 VM。

生产应使用独立构建节点；构建节点和消费节点通过配置一致的共享 POSIX/OSS 模板仓库分发。本文的 HostPath 仓库只能供同一 Worker 使用，不能把本地路径当成多节点共享仓库。

构建与消费均设置 `max_volume_count=8`、`storage_readonly_slots=2`；构建 Pod 不配置设备 Provider URL，且 `storage_required=false`。消费 Pod 始终 `storage_required=true`。不能为了构建模板临时关闭正在运行的消费节点授权。

### 10.2 启动临时构建 Pod

```bash
k apply --dry-run=server -f "$RUN/manifests/07-template-builder.json"
k apply -f "$RUN/manifests/07-template-builder.json"
k -n agentenv-ssf wait --for=condition=Ready pod/agentenv-template-builder --timeout=600s
k -n agentenv-ssf logs pod/agentenv-template-builder -c agentenv --tail=80
```

首次启动会先将镜像内工作区依赖补到独立 `/var/lib/aenv-builder`，模板仓库映射为 `/templates`。若 initContainer 或 AgentENV 下载依赖失败，查看对应容器日志；不要仅增加超时掩盖路径或网络错误。

### 10.3 使用原生模板 API 构建

只通过管理员 `kubectl exec` 访问构建 Pod 的 loopback API；没有为它创建可绕过适配器的原生 Service。下面的 key 展开在 Pod 内，不回传到管理终端。

```bash
builder_api() {
  local method="$1" path="$2"
  k -n agentenv-ssf exec -i pod/agentenv-template-builder -c agentenv -- \
    sh -c 'curl -fsS -X "$1" -H "X-API-Key: $AENV_API_KEY" \
      -H "Content-Type: application/json" --data-binary @- "http://127.0.0.1:8000$2"' \
    sh "$method" "$path"
}
printf '%s' '{"name":"ssf-ext4-template","cpuCount":2,"memoryMB":512}' \
  | builder_api POST /v3/templates > "$RUN/template-create.json"
export TEMPLATE_ID BUILD_ID
TEMPLATE_ID=$(jq -er '.templateID' "$RUN/template-create.json")
BUILD_ID=$(jq -er '.buildID' "$RUN/template-create.json")
jq -n --arg image "$GUEST_IMAGE" '{fromImage:$image}' \
  | builder_api POST "/v2/templates/$TEMPLATE_ID/builds/$BUILD_ID" \
  > "$RUN/template-start.json"
```

等待构建状态 `ready`。该脚本有明确期限；超时只表示仍需查询，不应换 ID 重建未知操作。

```bash
for attempt in $(seq 1 300); do
  builder_api GET "/templates/$TEMPLATE_ID/builds/$BUILD_ID/status?logsOffset=0" \
    < /dev/null > "$RUN/reports/template-status.json"
  status=$(jq -r '.status' "$RUN/reports/template-status.json")
  if [[ "$status" == ready ]]; then break; fi
  if [[ "$status" == error ]]; then
    cat "$RUN/reports/template-status.json"
    break
  fi
  sleep 2
done
jq -e '.status == "ready"' "$RUN/reports/template-status.json"
builder_api GET /v2/templates < /dev/null > "$RUN/reports/templates.json"
```

`/v3/templates` 返回的 `templateID` 是本次确定的 ID；将它固定到 catalog，不使用 `ssf-ext4-template` 这样的可变名字/alias。构建 API 的字段和路径来自 [AgentENV 模板客户端](../AgentENV/crates/aenv/src/client/templates.rs)。

实际 manifest 必须有 `volumeDriveSlots: 8`、`volumeReadonlySlots: 2`。可以只读查找刚生成的 manifest：

```bash
k -n agentenv-ssf exec pod/agentenv-template-builder -c agentenv -- \
  sh -c 'find /templates /workspace/env -type f -name firecracker-manifest.json -print' \
  > "$RUN/reports/template-manifest-paths.txt"
```

按本次 template ID 对应的仓库记录找到文件，使用 `kubectl exec ... -- cat ABSOLUTE_PATH` 导出，检查槽位及 CPU/内存记录。仓库可能有缓存和其他模板，不能随便取第一个文件。缺少只读槽位的老模板重新构建；手工改 manifest 不会改变 Firecracker 快照内真实设备属性。

### 10.4 关闭构建入口并登记 catalog

先确认模板构建完成、没有残留运行沙箱，保存日志：

```bash
builder_api GET /sandboxes < /dev/null > "$RUN/reports/builder-sandboxes.json"
k -n agentenv-ssf logs pod/agentenv-template-builder -c agentenv \
  > "$RUN/reports/template-builder.log"
# 检查上面的沙箱列表和原生日志，确认构建 VM 已停止后再执行：
k -n agentenv-ssf delete pod agentenv-template-builder --wait=true --timeout=180s
```

如果构建异常留下 VM，先用原生构建取消/停止流程处理并确认宿主进程退出，不能把 Pod 删除成功当作设备 fencing 的替代证据。正常构建完成后，在 Worker 只读核对没有该构建任务的残留 Firecracker；不要误杀其他 VM。

```bash
python3 - <<'PY'
import json, os, pathlib
p = pathlib.Path(os.environ['RUN'], 'control.json')
c = json.loads(p.read_text())
t = os.environ['TEMPLATE_ID']
c['templates'] = {t: {'id': t, 'resources': {'cpu': 2, 'memory_mib': 512},
                     'rw_slots': 6, 'ro_slots': 2}}
p.write_text(json.dumps(c, indent=2))
PY
render_install
k apply -f "$RUN/manifests/03-app-config.json"
```

重新渲染不会创建 Pod，也不会更换证书。catalog 的资源值必须与实际模板匹配；上述值与本节构建请求一致。模板实际设备规划还会复核槽位，不通过修改 catalog 绕过限制。

## 11. 启动节点和控制面

### 11.1 节点进程

```bash
k apply --dry-run=server -f "$RUN/manifests/08-workers.json"
k apply -f "$RUN/manifests/08-workers.json"
export NODE_SLUG
NODE_SLUG=$(python3 -c 'import os,re;print(re.sub("[^a-z0-9-]","-",os.environ["WORKER"].lower()))')
k -n agentenv-ssf rollout status "daemonset/aenv-$NODE_SLUG" --timeout=600s
k -n agentenv-ssf rollout status "daemonset/ssf-aenv-$NODE_SLUG" --timeout=180s
k -n agentenv-ssf get pods -o wide
k -n agentenv-ssf logs "daemonset/aenv-$NODE_SLUG" -c adapter --tail=80
k -n agentenv-ssf logs "daemonset/ssf-aenv-$NODE_SLUG" -c ssf-node --tail=80
```

若 DaemonSet 的 desired 为 0，优先检查 Node 名、taints、affinity 和 Pod admission。渲染器按节点名字固定调度，不自动为企业节点自定义 taint 添加 toleration。若 needed capacity 超过 allocatable，修改容量预算，而不是删除 requests。

Node 适配器 `/readyz` 会检查原生 AgentENV health；SSF Node 当前清单只有 liveness，DaemonSet Ready **并不验证完整 CSI/guest 链路**。创建真实沙箱仍是必要检查。

只读检查共享进程和设备路径：

```bash
k -n agentenv-ssf exec "daemonset/aenv-$NODE_SLUG" -c adapter -- \
  sh -c 'id; readlink /proc/self/ns/pid; ls -ld /var/lib/ssf /var/lib/agentenvadapter /var/lib/agentenv-external'
k -n agentenv-ssf exec "daemonset/aenv-$NODE_SLUG" -c agentenv -- \
  sh -c 'readlink /proc/self/ns/pid; ls -l /dev/kvm /dev/ublk-control'
k -n agentenv-ssf exec "daemonset/ssf-aenv-$NODE_SLUG" -c ssf-node -- id
```

同一 AgentENV Pod 的两个容器应返回同一个 PID namespace，SSF Node 应为 uid 0。SSF 镜像精简，未保证包含 curl/findmnt 等诊断工具；在 Worker 宿主或现有带工具的适配容器检查挂载，不临时安装工具改生产容器。

### 11.2 控制服务与入口

```bash
k apply --dry-run=server -f "$RUN/manifests/09-control.json"
k apply -f "$RUN/manifests/09-control.json"
k -n agentenv-ssf rollout status deployment/agentenv-control --timeout=180s
k -n agentenv-ssf logs deployment/agentenv-control --tail=80
k -n agentenv-ssf get svc
```

本地测试另开一个终端，使用相同 context，保持以下命令运行：

```bash
kubectl --context YOUR_TEST_CONTEXT -n agentenv-ssf \
  port-forward service/agentenv-control 9444:9444 --address 127.0.0.1
```

回到管理终端：

```bash
export API='https://127.0.0.1:9444'
client() {
  python3 "$ADAPTER/examples/client.py" --url "$API" \
    --ca "$RUN/pki/ca.crt" --cert "$RUN/pki/user.crt" --key "$RUN/pki/user.key" "$@"
}
api() {
  curl -fsS --cacert "$RUN/pki/ca.crt" --cert "$RUN/pki/user.crt" \
    --key "$RUN/pki/user.key" -H 'Content-Type: application/json' "$@"
}
```

不使用 `curl -k` 跳过证书验证。正式负载均衡入口需要匹配证书 SAN，优先 TCP/TLS 透传；在中间层终止 mTLS 后用一个共享客户端证书访问 control，会丢失原用户 URI 身份。不能随意将其改成普通 HTTP Ingress。

## 12. 冷启动、读写、删除与复用

### 12.1 创建 Immediate 块卷

```bash
cat > "$RUN/create-volume.json" <<'JSON'
{
  "namespace": "agentenv-demo",
  "name": "data-immediate-01",
  "storage_class": "ssf-block-immediate",
  "bytes": 1073741824,
  "initialize_ext4": true
}
JSON
client volume "$RUN/create-volume.json" > "$RUN/reports/volume.json"
export PVC_UID
PVC_UID=$(jq -er '.metadata.uid' "$RUN/reports/volume.json")
k -n agentenv-demo wait --for=jsonpath='{.status.phase}'=Bound \
  pvc/data-immediate-01 --timeout=180s
k -n agentenv-demo get pvc data-immediate-01 -o yaml > "$RUN/reports/pvc-before.yaml"
```

POST `/v1/volumes` 返回原生 PVC；返回成功不等于 Bound，也不等于已格式化。`initialize_ext4=true` 是对本次 API 创建、UID 固定的新空卷的显式初始化授权。实际初始化在取得独占消费权、Prepare 后进行；首次需要全卷空白检查，会增加延迟。已有 PVC 默认不能自动格式化。

同一创建参数可幂等重试。PVC 被删除后，适配表不会把同名新对象冒充旧卷，重新创建请换一个名字。

### 12.2 发起冷启动

```bash
jq -n --arg uid "$PVC_UID" --arg image "$GUEST_IMAGE" '{
  request_id:"cold-example-01", kind:"cold",
  launch:{image:$image,cpuCount:2,memoryMB:512,timeout:600},
  mounts:[{namespace:"agentenv-demo",pvc:"data-immediate-01",uid:$uid,
           id:"data",path:"/data",readonly:false}]
}' > "$RUN/create-cold.json"
client create "$RUN/create-cold.json" > "$RUN/reports/create-cold.json"
export OP
OP=$(jq -er '.id' "$RUN/reports/create-cold.json")
client inspect "$OP"
```

API 返回 `202` 和 operation ID。该 ID、适配 `instance`、原生 `sandbox_id` 是不同概念；查询、删除和 guest 代理都使用 **operation ID**。只有 `Ready` 才表示真实 guest 核验及 Consumption commit 已完成。

可用客户端现有 `wait()` 等待，而不是固定睡几秒：

```bash
wait_op() {
  python3 - "$OP" "${1:-Ready}" <<'PY'
import json, os, sys
sys.path.insert(0, os.path.join(os.environ['ADAPTER'], 'examples'))
from client import Client
r = os.environ['RUN']
c = Client(os.environ['API'], r+'/pki/ca.crt', r+'/pki/user.crt', r+'/pki/user.key')
print(json.dumps(c.wait(sys.argv[1], sys.argv[2], timeout=600), indent=2))
PY
}
wait_op Ready > "$RUN/reports/cold-ready.json"
client exec "$OP" 'printf "ssf-retained-data\n" > /data/probe.txt; sync; cat /data/probe.txt'
client exec "$OP" 'cat /proc/mounts | grep " /data "'
```

`client exec` 的返回码来自实际 guest 进程，验证目标是 `/data` 对应的数据卷。不能只在 guest 根盘写文件后就声称外部卷可用。

### 12.3 幂等、停止和释放

```bash
client create "$RUN/create-cold.json" > "$RUN/reports/cold-retry.json"
jq -r '.id,.instance' "$RUN/reports/create-cold.json" "$RUN/reports/cold-retry.json"
client delete "$OP" > "$RUN/reports/cold-delete-accepted.json"
wait_op Released > "$RUN/reports/cold-released.json"
k -n agentenv-demo get pvc data-immediate-01 -o json \
  | jq -e --arg uid "$PVC_UID" '.metadata.uid == $uid'
```

重复创建应返回同一 operation/instance；相同 `request_id` 改 mount 等参数应冲突。删除返回 202 只是接受停止请求；`Released` 才表示 VMM 停止、节点资源释放、末次 attachment 处理及计算预留释放。网络超时不表示这些动作已发生。

### 12.4 用同一 PVC 模板启动

```bash
jq -n --arg uid "$PVC_UID" --arg template "$TEMPLATE_ID" '{
  request_id:"template-example-01", kind:"template",
  launch:{templateID:$template,timeout:600},
  mounts:[{namespace:"agentenv-demo",pvc:"data-immediate-01",uid:$uid,
           id:"data",path:"/data",readonly:false}]
}' > "$RUN/create-template.json"
client create "$RUN/create-template.json" > "$RUN/reports/create-template.json"
OP=$(jq -er '.id' "$RUN/reports/create-template.json")
wait_op Ready > "$RUN/reports/template-ready.json"
client exec "$OP" 'cat /data/probe.txt'
client delete "$OP"
wait_op Released > "$RUN/reports/template-released.json"
```

应读回 `ssf-retained-data`。这个步骤证明“删除沙箱不删除 PVC，模板重建重新消费同一个卷”；它不意味着对带外部卷的运行实例做了 snapshot/fork。

## 13. WFFC、静态 PV 与只读验收

### 13.1 动态 WFFC

创建另一份 VolumeRequest，将 `name` 改成 `data-wffc-01`，`storage_class` 改成 `ssf-block-wffc`，保留首次初始化授权。**创建 PVC 后不执行前面的 Bound 等待命令**，先观察 Pending，再用新 request_id 发起沙箱创建：

```bash
jq '.name="data-wffc-01" | .storage_class="ssf-block-wffc"' \
  "$RUN/create-volume.json" > "$RUN/create-wffc-volume.json"
client volume "$RUN/create-wffc-volume.json" > "$RUN/reports/wffc-volume.json"
k -n agentenv-demo get pvc data-wffc-01 -o yaml > "$RUN/reports/wffc-before.yaml"
WFFC_UID=$(jq -er '.metadata.uid' "$RUN/reports/wffc-volume.json")
jq --arg uid "$WFFC_UID" '.request_id="wffc-example-01" |
  .mounts[0].pvc="data-wffc-01" | .mounts[0].uid=$uid' \
  "$RUN/create-cold.json" > "$RUN/create-wffc.json"
client create "$RUN/create-wffc.json" > "$RUN/reports/create-wffc.json"
OP=$(jq -er '.id' "$RUN/reports/create-wffc.json")
wait_op Ready > "$RUN/reports/wffc-ready.json"
k -n agentenv-demo get pvc data-wffc-01 -o yaml > "$RUN/reports/wffc-after.yaml"
client exec "$OP" 'echo wffc-ok > /data/check; sync; cat /data/check'
client delete "$OP"
wait_op Released
```

预期：创建 PVC → Pending → 适配预留计算节点并获得选点许可 → selected-node/原生供卷绑定 → 校验真实 PV 拓扑 → Attach/Prepare/guest Ready。Pending 或未知供卷结果期间不手工改 selected-node、删 PVC 或换节点“重试”。

Kubernetes 的 WFFC 会把供卷/绑定延后到消费者相关的调度信息确定之后；本实现为独立 VM 提供选点协调，不靠逐沙箱 Pod 触发。[Kubernetes Volume Binding Mode](https://kubernetes.io/docs/concepts/storage/storage-classes/#volume-binding-mode)

### 13.2 静态 PV 与预绑定 PVC

静态流程需要管理员提供 **已存在、可供该 Driver 识别、已有 ext4 的独立测试后端卷**。这个前提由存储后端/厂商流程完成，不在消费端调用 CreateVolume 或格式化已有 PVC。loop fixture 的 volumeHandle 必须来自其真实登记，不能随意编造一个字符串套进 YAML。

下面是资源模板，替换真实 driver、handle、容量和拓扑后才应用。`nodeAffinity` 示例只表示单节点本地卷；远端卷按真实 zone/region 约束填写，不能照抄本地节点约束。`csi.volumeAttributes` 和各 SecretRef 也按厂商要求提供。

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata: {name: ssf-static-immediate}
provisioner: kubernetes.io/no-provisioner
reclaimPolicy: Retain
volumeBindingMode: Immediate
---
apiVersion: v1
kind: PersistentVolume
metadata:
  name: ssf-static-data-01
  labels: {ssf.dev/managed: 'true'}
spec:
  capacity: {storage: 1Gi}
  volumeMode: Block
  accessModes: [ReadWriteOnce]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: ssf-static-immediate
  claimRef: {namespace: agentenv-demo, name: static-data-01}
  csi:
    driver: REPLACE_WITH_REAL_CSI_DRIVER
    volumeHandle: REPLACE_WITH_EXISTING_HANDLE
  nodeAffinity:
    required:
      nodeSelectorTerms:
        - matchExpressions:
            - key: kubernetes.io/hostname
              operator: In
              values: [REPLACE_WITH_REAL_HOSTNAME_LABEL_VALUE]
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: static-data-01
  namespace: agentenv-demo
  labels: {ssf.dev/managed: 'true'}
spec:
  volumeName: ssf-static-data-01
  storageClassName: ssf-static-immediate
  volumeMode: Block
  accessModes: [ReadWriteOnce]
  resources: {requests: {storage: 1Gi}}
```

节点 hostname label 未必等于 Node 名，先查询实际值。预绑定也不能跳过真实拓扑校验。等待 Bound 后，创建沙箱请求直接引用该 PVC，并固定当前 UID；不要对已有对象调用 `POST /v1/volumes` 来补录 `initialize_ext4=true`。

静态 WFFC：创建另一个 `volumeBindingMode: WaitForFirstConsumer` 的 `kubernetes.io/no-provisioner` 类；准备受管、Retain、Available、**没有 claimRef** 的同类 PV；PVC 不设置 `volumeName`。首次消费选点后，公共协调器选择满足容量/模式/标签/真实拓扑的静态候选并绑定。若预先设置 `volumeName`/claimRef，测试的就是显式预绑定，不足以覆盖延迟选卷逻辑。

### 13.3 只读

确认先前 writer 已 `Released`，以新 request_id 将 mount `readonly` 改为 true；分别用 cold 和 template 运行。读原数据成功，下面写操作必须返回非零：

```bash
# OP 必须是本轮已经 Ready 的只读 operation。
client exec "$OP" 'cat /data/probe.txt'
if client exec "$OP" 'echo forbidden > /data/should-not-exist'; then
  printf '%s\n' 'FAIL: readonly write unexpectedly succeeded'
else
  printf '%s\n' 'Check guest stderr: failure must be caused by readonly storage'
fi
```

不能将网络断开、权限不足或路径不存在算作只读通过。实现同时核验宿主设备、VMM 文件描述符/设备属性和 guest 挂载属性；模板必须有真实 RO 槽位。脏 ext4 需要日志恢复时只读启动可能拒绝，应先用获授权的正常读写挂载恢复并干净停止；不强制重格式化，也不跳过一致性检查。

### 13.4 显式回收 PVC

删除沙箱和删除卷是两个 API。先确认所有关联 operation 都 Released，再显式请求 Retain 或 Delete，固定 PVC UID：

```bash
api -X DELETE "$API/v1/volumes/agentenv-demo/data-immediate-01?uid=$PVC_UID&reclaim=Retain"
```

按同一 UID/action 重试查询推进，结合 Kubernetes PVC/PV 状态判断。`ReclaimPending` 是尚未完成；`Retained` 表示 PVC 已删除且 PV 处于保留状态。`ClaimDeleted` 只证明原生对象删除已观察到，不证明后端执行了 DeleteVolume。

如需真实删除后端，明确使用 `reclaim=Delete`；适配器只排空/授权并修改原生资源，由 provisioner/Driver 完成后端回收。检查原 PV、provisioner 日志和存储后台结果，不能用 HTTP 202 当作数据已擦除。已经开启 Delete 的流程不能再承诺 Retain。

对第 8 节没有绑定的 probe 也通过 API 清理：

```bash
PROBE_UID=$(k -n agentenv-demo get pvc guard-probe -o jsonpath='{.metadata.uid}')
api -X DELETE "$API/v1/volumes/agentenv-demo/guard-probe?uid=$PROBE_UID&reclaim=Retain"
```

## 14. 自动化验收与性能报告

### 14.1 分开记录测试层次

| 层次 | 执行工具/流程 | 能证明什么 | 不能证明什么 |
| --- | --- | --- | --- |
| Go 单元/契约 | `go test`、race、vet | 接口、权限、签名、状态约束等本机检查 | Linux 挂载、Rust 类型正确或 guest 成功 |
| PostgreSQL | `linux-integration.sh` 中的真实 PG 测试 | 并发幂等、预留、writer/删除竞争 | 真实 CSI 与 VM |
| Linux 挂载 | 同一脚本中的 privileged fixture 测试 | 真实 Node 挂载/设备路径及释放 | 厂商 Driver、Firecracker 完整闭环 |
| Kubernetes | 本文及故障矩阵 | Guard、UID、拓扑、静态/Immediate/WFFC | VM 数据面仅靠对象状态不能证明 |
| 真实 VM | `vm-e2e.py` | 冷启动/模板 RW/RO、删除重建数据保持 | 所有故障恢复、生产存储 SLA |
| 厂商 Driver | 相同场景另跑另报 | 对指定 Driver/版本/后端的兼容性 | 其他厂商或版本 |

### 14.2 PostgreSQL 和 Linux 测试

在独立 Linux 测试主机执行；会启动临时 Docker PostgreSQL、运行真实挂载测试并构建 AgentENV，不对本文应用数据库做 destructive 测试。

```bash
bash "$ADAPTER/tests/preflight.sh"
bash "$ADAPTER/tests/linux-integration.sh"
```

preflight 同时要求编译工具、Docker、kubectl 和设备，适合一体化测试机。如果管理机/构建机/Worker 分离，分别满足对应角色要求，不能在 macOS preflight 失败后宣布 Linux 节点不兼容。

脚本 PostgreSQL 容器是一次性测试数据，结束清理；它不替换集群共享数据库。测试输出在 `adapter/agentenvadapter/validation/linux/`。Linux 脚本成功之后还要单独运行 VM E2E。

### 14.3 真实 VM 主流程

保证本地 port-forward 持续运行，当前模板已登记，容量允许并发。第一次少量运行，成功后再扩大样本：

```bash
python3 "$ADAPTER/tests/vm-e2e.py" \
  --url "$API" --ca "$RUN/pki/ca.crt" \
  --cert "$RUN/pki/user.crt" --key "$RUN/pki/user.key" \
  --namespace agentenv-demo --class ssf-block-immediate \
  --image "$GUEST_IMAGE" --template "$TEMPLATE_ID" \
  --runs 1 --concurrency 1 --report "$RUN/reports/vm-immediate.json"

python3 "$ADAPTER/tests/vm-e2e.py" \
  --url "$API" --ca "$RUN/pki/ca.crt" \
  --cert "$RUN/pki/user.crt" --key "$RUN/pki/user.key" \
  --namespace agentenv-demo --class ssf-block-wffc \
  --image "$GUEST_IMAGE" --template "$TEMPLATE_ID" \
  --runs 1 --concurrency 1 --report "$RUN/reports/vm-wffc.json"
```

每个 scenario 自动创建新 PVC，依次运行 cold RW → template RW → cold RO → template RO，校验重复请求 instance 一致、真实写读和只读失败、删除后 PVC UID 保持。脚本不会删除测试 PVC。

已有静态 ext4 测试卷使用下面的参数；**会在该卷写入 `/data/retained`**，只使用获授权的独立测试卷：

```bash
python3 "$ADAPTER/tests/vm-e2e.py" \
  --url "$API" --ca "$RUN/pki/ca.crt" \
  --cert "$RUN/pki/user.crt" --key "$RUN/pki/user.key" \
  --namespace agentenv-demo --image "$GUEST_IMAGE" --template "$TEMPLATE_ID" \
  --existing-pvc static-data-01 --runs 1 --concurrency 1 \
  --report "$RUN/reports/vm-static.json"
```

失败时脚本输出 FAIL/错误并可能保留待恢复操作。保存 stdout/stderr 和 operation ID；完整查询/释放后再重跑，不用删除 Pod 或 SQL 行清场。

### 14.4 故障和性能

按 [故障验收矩阵](../SSF/adapter/agentenvadapter/tests/fault-matrix.md) 在隔离环境逐项注入：迟到启动、取消竞争、Hook 丢失、各进程重启、整个 Pod 替换、Host reboot、Node UID 重建、多卷失败和最后引用释放。主流程测试不自动涵盖这些注入。

样本充足时提高 `--runs`，并按实际 capacity 设置 `--concurrency`。本文 4 CPU 的预算、每 VM 2 CPU，可先测试并发 2；不要照搬并发 4 后把容量等待误记为存储瓶颈。分别归档首次供卷/首次格式化、已 Bound 已格式化复用、冷启动、模板启动及不同并发量。

现有报告输出 cold/template 的 p50/p95/p99 和实际阶段累计耗时。`overall_ms` 包含客户端 50 ms 轮询量化；Prepare 可能含初始化，stage 耗时可能含重试。它不是纯 Firecracker 启动时间，也不能将小样本 p99 当作稳定 SLA。测试 Driver 与厂商 Driver 分开保存报告；没有数据不填估计延迟。

## 15. 诊断、升级、恢复与卸载

### 15.1 先定位 operation，再查各层

```bash
client inspect "$OP" > "$RUN/reports/operation-$OP.json"
k -n agentenv-ssf get pods -o wide
k -n agentenv-ssf logs deployment/agentenv-control --since=10m
k -n agentenv-ssf logs "daemonset/aenv-$NODE_SLUG" -c adapter --since=10m
k -n agentenv-ssf logs "daemonset/aenv-$NODE_SLUG" -c agentenv --since=10m
k -n agentenv-ssf logs "daemonset/ssf-aenv-$NODE_SLUG" -c ssf-node --since=10m
k -n ssf-fixture logs daemonset/ssf-block -c driver --since=10m
k -n ssf-fixture logs daemonset/ssf-block -c gateway --since=10m
k -n ssf-system logs deployment/ssf-guard --since=10m
k -n agentenv-demo get events --sort-by=.metadata.creationTimestamp
```

Deployment 多副本日志命令默认不保证覆盖全部副本；必要时按 Pod 名分别导出。保留原请求、operation、instance、sandbox_id、PVC/PV UID、Node UID、Boot ID、真实 CSI node_id 及时间，切勿只凭 PVC 名对应故障。

数据库只读诊断示例：

```bash
k -n ssf-system exec deployment/ssf-postgres -- psql -U postgres -d ssf -c \
  "SELECT id,tenant,instance,state,stopping,sandbox_id,reason,updated FROM agentenv_adapter_operations ORDER BY updated DESC LIMIT 30;"
k -n ssf-system exec deployment/ssf-postgres -- psql -U postgres -d ssf -c \
  "SELECT operation,node,node_uid,boot_id,cpu,memory_mib,state,expires FROM agentenv_adapter_reservations WHERE state <> 'Released';"
k -n ssf-system exec deployment/ssf-postgres -- psql -U postgres -d ssf -c \
  "SELECT volume_key,node,node_id,state FROM ssf_attachments;"
```

节点 bbolt 不要用第二个进程以读写方式同时打开，也不要拷贝正在变化的数据库文件充当一致备份。使用正常停写/进程停止后的备份或已有诊断接口。凭据、签名计划、包含敏感参数的日志归入受限运维材料。

### 15.2 常见问题

| 现象 | 检查与处理 |
| --- | --- |
| Pod Pending / desired=0 | Node 名、affinity、taints/tolerations、资源余量、PSA；不要删除容量约束绕过 |
| ImagePullBackOff | 各 Namespace 的 imagePullSecrets、镜像架构、镜像是否进入 kubelet 实际使用的 CRI |
| 原生启动时重新下载依赖 | `/workspace` HostPath 是否正确预置；initContainer 日志、依赖清单与镜像一致性 |
| SSF mount permission denied | SSF Node 是否 uid 0 + privileged，HostPath 与传播设置是否正确 |
| mTLS handshake / 403 | CA、有效期、DNS SAN、唯一 URI SAN、角色 allowlist、tenant/namespace；不用 `-k` 绕过 |
| Guard TLS / 调用超时 | Guard Pod、EndpointSlice、caBundle、Service DNS、API Server 到 Webhook 网络与数据库 |
| control 报 relation missing | 迁移 Job 是否 Complete、是否连到相同数据库、schema/search_path 和权限 |
| Immediate 长期 Pending | provisioner/Driver 日志、容量、真实后端与 StorageClass；不要先假定一个节点 |
| WFFC 长期 Pending | 原始 operation、预留、选点许可、selected-node、provisioner 或静态候选拓扑；不换节点重建未知结果 |
| Attach 失败 | Gateway 账本授权、真实 node_id、Driver 能力、Secret、网络、后端 attachment 状态 |
| Prepare 成功但 VM 未 Ready | 原生设备接入、VMM FD、guest UUID/容量/readonly/ext4；Node Publish 成功不是 guest 成功 |
| 模板启动拒绝 | catalog 的确定 ID/资源、实际 manifest RW/RO 槽位、模板仓库可见性、CPU/Firecracker 兼容 |
| filesystem not blank / UUID 不符 | 这是已有或未知初始化状态；保留 intent，核对真实设备，不强制 mkfs |
| Stopping 长期不 Released | VMM 是否仍用设备、原生 stop journal、PID namespace/Boot ID、Evidence 校验、末次引用 |
| PVC 删除被 Guard 拒绝 | 仍有 holder 或未授权排空；走适配 DELETE，不移除 managed 标签或绕开 webhook |
| 同名 PVC 被拒绝 | 固定 UID 与当前 UID 不一致；不能给旧操作换新卷 |
| bbolt locked | 是否同节点重复部署了通用 ssf-node，或启动了第二个适配进程；先核对唯一进程归属 |

指标端口可通过管理员 port-forward 检查：control/node 的 9091 提供 `/livez`、`/readyz`、`/metrics`；SSF 的 9090 提供 `/livez`、`/metrics`，不要请求不存在的 SSF HTTP `/readyz`。NetworkPolicy 没有开放公共指标采集入口，接入 Prometheus 时按指定采集者补规则。

### 15.3 正常升级和排空

1. 在统一入口暂停新的 POST 创建请求，保留 GET/DELETE 和后台恢复。当前适配器没有独立的管理员 Drain API；入口治理由部署平台完成。Placement 会跳过新预留时已经 Unschedulable 的节点，但 **`kubectl cordon` 不会撤销已有预留或完成排空**。也不要直接删 worker 配置，因为旧操作仍需路由到原节点完成释放。
2. 用元数据表只读列出未终态操作；按租户权限调用 DELETE 并等待 Released，同时核对 SSF holder/attachment 及原生进程。租约过期不等于已释放。
3. 备份数据库、节点日志、配置、模板和密钥；保持彼此对应的版本。先在测试环境验证迁移和 AgentENV 补丁。
4. 更新镜像/config。本文节点 DaemonSet 为 OnDelete，只有确认该节点已排空，才删除对应的旧 Pod 使新版本落地。不能对运行中的 AgentENV 节点直接执行无排空滚动重启。
5. 验证相同 Node UID/新进程身份、CSI 和模板能力，跑一轮冷/模板读写和释放，再恢复创建入口。

control 配置和 TLS 文件在启动时加载，改 ConfigMap/Secret 不代表进程已采用新值。控制面可在版本兼容的前提下逐副本更新；节点更新遵守排空。计划公钥当前不是多 key 自动轮换机制，轮换时排空旧计划及恢复操作，协调发布新私钥、公钥和所有使用方。

### 15.4 崩溃恢复边界

只重启一个进程且保留同 Pod/PID namespace、WAL，可按原 operation 恢复。整个 AgentENV Pod 被替换但宿主 Boot ID 未变时，新适配器不能根据“看不见旧 PID”证明旧 VMM 已退出；保留引用并诊断。

必要时通过受控宿主 fencing（例如确认受影响 VM 后重启宿主）建立停止证据。同名 Node 被删除重建后 UID 变化也不能自动当作旧节点可信。恢复不会自动恢复 VM 内存；没有“删日志/改 SQL 即强制解锁成功”的受支持流程。

数据库与节点日志不一致时先停止新消费，保全两边证据，对具体实例和真实后端状态核对。不可回滚数据库快照后让仍运行的 VM 继续使用已经被遗忘的 writer 权限。

### 15.5 卸载顺序

先冻结创建，逐个停止沙箱并等 Released；再按需要显式 Retain/Delete 各 PVC，记录保留 PV/后端资产。确认没有待恢复引用后：

```bash
# 仅在上面排空、回收/资产登记完成后执行。
k delete -f "$RUN/manifests/09-control.json"
k delete -f "$RUN/manifests/08-workers.json"
```

测试 fixture 在所有卷回收完成后才能卸载；厂商 Driver 如果还服务其他用户不删除。**Guard/webhook 最后处理**：先证明集群没有需要保护的 SSF 资源/消费，再删除 webhook，最后删除 Guard。不能先停 Guard 留下 fail-closed webhook 造成集群资源请求阻塞。

数据库、模板、HostPath、PV 和 Secret 不随本手册自动清空。归档后由管理员按资产和数据保留策略处理，不提供一键 `rm -rf` 或全 Namespace 删除来代替排空。

## 16. 生产替换与多节点部署

### 16.1 厂商 CSI 接入

主线安装渲染器只为了单 Worker 实验；生产使用原 [适配渲染器](../SSF/adapter/agentenvadapter/scripts/render.py) 输出项目部分，并由平台维护各基础设施的正式清单。仍要保留本文 root 身份、依赖预置、排空更新和模板分发设置。

| 替换项 | 具体配置/核验 |
| --- | --- |
| Driver 注册名 | control `drivers[].Name` 必须等于 PV `spec.csi.driver`、CSINode 条目和 Driver Identity 名 |
| Controller 入口 | `ControllerEndpoint` 为受信 TLS endpoint；只有 UDS 时将 Gateway 置于该 Controller Pod 并共享 socket |
| Gateway | `DriverName`、`DriverEndpoint`、AllowedPeers、CA/证书/Service SAN；共享同一个 SSF 数据库；不开放管理 Create/Delete RPC |
| Node socket | 原渲染器默认 `/var/lib/kubelet/plugins/<driver>/csi.sock`，厂商实际不同时改生成的每节点配置和 HostPath；不要伪造 socket |
| 配置版本 | controller/node 的 `Revision` 一致，按实际 Driver/配置变更管理版本；不能复用旧语义的计划 |
| StorageClass | 专门的 Retain 类，Block 能力，Immediate/WFFC，真实 parameters/allowedTopologies/Secret 模板 |
| CSI Secret | Controller/Node 阶段所需 SecretRef 由原生 PV/StorageClass 正确传递；服务 SA 只能读必要 Secret |
| 拓扑和容量 | 真实 node_id、CSINode、node labels、PV affinity、后端限制与专用池计算容量一起验证 |
| 原生消费者隔离 | 受管卷经 Guard 阻止原生 Pod 同时消费；不能让 kubelet/native VolumeAttachment 与 SSF 竞争同一使用操作 |
| 真实后端能力 | Block、Stage/Publish、readonly、attach/attachless 能力组合、超时重试幂等、同节点多 holder 的实际语义 |

普通厂商 CSI 容器可以继续被 Kubernetes sidecar 和 SSF 客户端分别调用标准协议；兼容协议不等于厂商默认支持此消费模型。需单独验证其鉴权、Secret、拓扑、Pod 专用参数假设和节点持久状态。

### 16.2 数据库账号、TLS 与备份

生产连接同一个 PostgreSQL 数据库/schema，但拆分登录角色：

- 迁移 owner：只供部署 Job 使用，拥有 DDL 权限。
- control：公共 `ssf_consumer` 组权限，再加适配表所需 DML。
- Guard：公共 `ssf_guard_reader`。
- Gateway：公共 `ssf_gateway_reader`。
- 节点适配器/SSF Node：不需要 PostgreSQL 凭据。

迁移后由 DBA 应用 [公共角色 SQL](../SSF/deploy/ledger-roles.sql)，再补适配组权限：

```sql
CREATE ROLE agentenv_adapter_consumer NOLOGIN;
GRANT USAGE ON SCHEMA public TO agentenv_adapter_consumer;
GRANT SELECT, INSERT, UPDATE ON
  agentenv_adapter_operations,
  agentenv_adapter_reservations,
  agentenv_adapter_volumes
TO agentenv_adapter_consumer;
-- 由 DBA 另建使用安全认证的 LOGIN 账号，然后按角色赋权。
-- GRANT ssf_consumer, agentenv_adapter_consumer TO control_login;
-- GRANT ssf_guard_reader TO guard_login;
-- GRANT ssf_gateway_reader TO gateway_login;
```

当前表路径主要使用 INSERT/UPDATE 保留 tombstone，不能假设删除历史行没有语义影响。按实际 SQL 及升级迁移复查授权，不把测试 postgres 超级用户带进生产。把 migration Job 的 env Secret 改成单独迁移凭据，control/Guard/Gateway 各用自己的 Secret。

使用 `sslmode=verify-full` 的 PostgreSQL DSN，挂入数据库 CA（与应用 mTLS CA 可不同）并指定 `sslrootcert`，验证服务 DNS。备份覆盖 `ssf_*` 和全部适配表，并建立与节点 WAL/原生日志及后端快照的一致恢复方案。单机 HostPath PostgreSQL 只是实验依赖，不能承担高可用承诺。

### 16.3 多节点与模板分发

1. 在 control `workers` 中登记每个真实 Node 名、URL 和可用容量。原渲染器按 Worker 输出独立 DS/Service/config；同一个节点不要重复部署两个适配实例。
2. 为每个 node/ssf-node 签发独立 URI/DNS 证书。检查不同 Node 名经 slug 规范化后不冲突、生成的资源/DNS 名长度合法；渲染器不会替平台完成全部命名审查。
3. 选择真实支持跨节点/拓扑的厂商 Driver。本文本地 loop fixture 和 HostPath 模板仓库都不能直接横向复制成共享后端。
4. 在构建和消费配置中使用相同 POSIX 共享目录或 AgentENV OSS backend；同时分发完整模板记录、manifest、VM state、memory/rootfs artifacts，而非只拷一个 manifest。模板 CPU/内核/Firecracker 兼容仍需实际验证。
5. 独立构建节点不加入适配 workers 容量池；消费节点保持原生入口 loopback 和外部启动授权。网络/RBAC 限制原生 API key、特权 Pod exec 和主机访问。
6. 若给节点池加 taint，显式给 AgentENV、适配器、SSF Node、厂商 CSI 配置正确 toleration；其他工作负载不能无约束进入。主线没有自动修改全局 taint。

控制面两副本共享数据库不是数据库高可用；也不能仅凭 Deployment replicas=2 断言所有重试/故障场景已验收。逐项跑真实多节点故障矩阵后再开放生产。

## 17. 交付检查与执行记录

### 17.1 安装完成判据

| 判据 | 记录内容 |
| --- | --- |
| 源码及镜像一致 | AgentENV 基线/补丁 SHA、SSF 版本、镜像 digest、Rust/Go 构建结果 |
| 基础设施可用 | Node UID/Boot ID、CSINode、数据库迁移、Guard 实际拒绝测试、证书到期时间 |
| 创建路径完整 | 原始请求、PVC/PV UID、operation/instance/native ID、guest 真实读写结果、Ready |
| 释放路径完整 | stop 证据、Released、末次 Unpublish/Unstage/Detach 顺序、无遗留 VMM 引用、PVC 保留 |
| 模式覆盖 | 冷/模板 × RW/RO；静态预绑定、动态 Immediate、动态/静态 WFFC |
| 故障恢复 | 矩阵各项实际命令、注入点、恢复日志与结果；未执行项写 NOT RUN |
| 性能 | 样本数、并发、初次格式化/复用状态、完整硬件及 Driver 版本；分组 p50/p95/p99 |

建议以 [报告模板](../SSF/adapter/agentenvadapter/tests/report-template.json) 为起点，报告独立保存到 `$RUN/reports`，不用手工把仓库历史摘要改成 PASS。

### 17.2 本次文档交付的验证边界

本次新增部署手册及离线安装覆盖渲染器；不修改 AgentENV、SSF 核心或现有适配业务实现，不修改 K8S CSI 架构分析原文。辅助脚本只提供可审查的部署清单，不等于自动安装器。

本次文档检查记录见 [安装文档检查报告](../SSF/adapter/agentenvadapter/validation/installation-doc-checks.json)。既有软件检查状态仍以 [2026-10-19 实现验证摘要](../SSF/adapter/agentenvadapter/validation/summary.json) 为准：Go 单测/race/vet、协议和补丁检查已执行；Rust 类型检查、PostgreSQL、集群、Linux 挂载、真实 VM、故障和性能项未在当前主机实跑。

完成本文部署后，由实际执行者逐项填写证据，再把该环境标为已验证。某个阶段未完成时停在对应检查点，保留状态，不把后续命令的存在当作该能力已经通过。
