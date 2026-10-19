# SSF AgentENV：真实环境端到端测试方案与操作手册

日期：2026-10-19。对象：当前 SSF 公共框架、`agentenvadapter`、AgentENV v0.2.3 外部设备补丁、真实 Kubernetes/PostgreSQL/CSI/Firecracker/guest。

本文独立定义场景、自动化入口、人工操作、故障注入、证据和通过标准。环境安装按 [端到端部署手册](13-agentenvadapter-installation-and-deployment.md) 完成。**本文是待执行的真实环境测试方案，不是通过报告。当前 macOS 仅执行脚本本身的离线检查，未执行集群、Linux、Rust、VMM 或厂商 Driver 测试。**

## 1. 测试目标、范围与判定原则

测试从真实租户证书和创建请求开始，经过计算预留、PVC/PV 绑定、消费账本、Attach、SSF Node、VMM 和 guest，最后验证释放和数据保留。验收对象不是“API 返回 202”或“Pod Ready”，而是完整数据和资源生命周期。

必须分别记录：

1. `PVC Bound`：Kubernetes 管理对象完成绑定。
2. attachment：后端设备可以提供给指定真实 CSI node_id。
3. host Stage/Publish：宿主可用设备与持久日志。
4. guest Ready：指定实例看到了正确 UUID、容量、ext4、挂载点和只读属性。
5. `Released`：运行时停止证据成立、节点与 attachment/预留按引用顺序释放。
6. Retain/Delete：显式卷回收的结果；删除沙箱不隐式删 PVC。

这些状态不能互相替代。客户端超时、HTTP 404、租约过期、Pod 被删除、进程在新 PID namespace 中不可见，都不能单独作为停止/fencing 证据。

当前适配支持新增 CSI Block/ext4 卷的冷启动和模板启动。带外部卷的暂停/恢复、snapshot、fork、在线扩容、FileTree 是**拒绝能力测试**，不能列为已支持功能。卷管理唯一走原生 Kubernetes 资源和 CSI 配套组件，测试不让 Consumption 直接 CreateVolume/DeleteVolume。

### 1.1 结果状态

| 状态 | 含义 |
| --- | --- |
| PASS | 该具体断言有真实证据；同时标明覆盖边界 |
| FAIL | 行为违反预期，例如双 writer、数据改变、只读写成功、未排空删除成功 |
| BLOCKED | 依赖/环境/可观测性不足或结果仍未知，不能给 PASS |
| NOT_RUN | 未执行；不能因为接口/脚本存在而改成 PASS |
| NOT_HIT | 故障注入没有命中要求的时间窗口，需重做，不能当故障通过 |

自动化总状态 `PASS_SELECTED_CASES` 只代表选中的场景断言通过；未选择场景保持 NOT_RUN。即使退出码为 0，也不代表故障矩阵、厂商 Driver、多节点或生产认证通过。报告固定保留 `approved_for_production: false`。

## 2. 环境分层与测试数据

### 2.1 至少准备两套执行配置

| 环境 | 作用 | 必要条件 |
| --- | --- | --- |
| E1：单 Worker 真实 Linux 实验环境 | 首次贯通、真实 loop Block Driver、故障和恢复 | Linux amd64/KVM/ublk，按部署手册安装；测试 CSI Controller/Node 固定同一节点 |
| E2：厂商存储环境 | 指定 Driver/后端的真正兼容性、拓扑和性能 | 厂商 CSI、实际存储网络/鉴权/Secret、支持 Block；使用独立报告 |
| E3：至少两 Worker/两个拓扑域 | 联合拓扑、节点不可用、迁移限制 | 真实支持对应拓扑的后端；节点容量和模板分发均就绪 |

E1 的 loop 文件不证明 E2 的远端存储能力，默认 kind 不证明 KVM/ublk/真实 guest。先 E1、再 E2；只有 E3 能有效验证跨节点/跨 zone 选择。已有集群场景按实际部署记录版本，不把本文测试基线称为所有版本兼容性认证。

### 2.2 执行前检查点

- 按 [部署手册环境章节](13-agentenvadapter-installation-and-deployment.md#2-环境与版本) 检查设备、挂载传播、共享 PID namespace、持久目录、镜像架构及版本。
- 原生 AgentENV API/Hook 只监听 Pod loopback；只有适配入口可以向专用池创建实例。测试机使用有效 mTLS 用户证书。
- PostgreSQL 迁移成功；Guard/webhook 已实际验证，不只是健康检查通过。
- Immediate/WFFC 类均 Retain，卷模式 Block；Driver 在 CSINode 注册真实 node_id。
- 模板为固定不可变 ID，CPU/内存及实际 RW/RO 槽位匹配；guest 有 `/bin/sh`、`head`、`tr`、`sync`、`sha256sum`、`awk`。
- writer/共享 reader 测试至少保留两个 VM 的计算容量，例如单 VM 2 CPU/512 MiB 时，专用池至少有 4 CPU 可用配额。不能把“计算容量不足”误判为 writer 排他成功。
- 仅使用独立测试 Namespace、测试 PVC 和可丢弃的业务数据。故障测试在隔离环境执行，事先保存原配置和恢复命令。
- 首轮磁盘预算：默认自动场景约创建十余个 1 GiB PVC；性能测试每个 run 再建一个 PVC。脚本默认保留 PVC，按实际 run 数量准备容量。

### 2.3 数据和身份

每次执行分配唯一 run_id，每个请求、卷和结果文件带此标识。记录 tenant、request_id、适配 operation ID、适配 instance UID、原生 sandbox_id、PVC/PV UID、Driver/handle、Node UID/Boot ID、VMM PID/start time 和设备 major:minor。

自动化在每个真实 guest 数据卷写入 1 MiB 确定模式数据和随机标记，以 SHA-256 验证删除重建后的内容；多卷使用不同标记。已有静态卷只写本次 run 命名的测试文件，不格式化；但它仍是写测试，必须是专门的测试卷。

## 3. 交付脚本与调用方式

代码统一位于 [tests/realenv](../SSF/adapter/agentenvadapter/tests/realenv/README.md)，不修改 SSF 核心和 AgentENV 业务源码。

| 文件 | 用途 |
| --- | --- |
| [suite.py](../SSF/adapter/agentenvadapter/tests/realenv/suite.py) | 14 个可选真实场景；只调用真实 HTTPS API、Kubernetes 和 guest；没有 mock 成功模式 |
| [config.example.json](../SSF/adapter/agentenvadapter/tests/realenv/config.example.json) | 环境、身份、类、镜像、模板、超时、并发和静态卷配置 |
| [collect.py](../SSF/adapter/agentenvadapter/tests/realenv/collect.py) | 只读采集节点/CSI/PVC/PV/webhook/Pod 状态和各容器日志；不读 Secret 或 ConfigMap 内容 |
| [device-scan.py](../SSF/adapter/agentenvadapter/tests/realenv/device-scan.py) | Linux 宿主扫描指定 major:minor 的打开块设备 FD；辅助核对孤儿引用 |
| [host-signal.py](../SSF/adapter/agentenvadapter/tests/realenv/host-signal.py) | Linux 宿主按 Boot ID、PID/starttime、exe 和 pidfd 定点注入 TERM/KILL/短暂停顿 |
| [test_tools.py](../SSF/adapter/agentenvadapter/tests/realenv/test_tools.py) | 仅检查测试工具的身份计算、脱敏、断言和统计分组，不能替代真实环境验收 |

原有 [vm-e2e.py](../SSF/adapter/agentenvadapter/tests/vm-e2e.py) 可继续作四轮数据烟雾测试；本方案的新 suite 增加场景、持久请求意图、失败记录和清理边界，不覆盖或美化已有历史报告。

### 3.1 管理端初始化

所有命令使用 Bash，除注明“Worker 宿主”外，都在 `sandboxVolume/SSF` 执行。不要依赖上一份文档终端里的临时变量。

```bash
cd /path/to/sandboxVolume/SSF
export TOOLS="$PWD/adapter/agentenvadapter/tests/realenv"
export ADAPTER="$PWD/adapter/agentenvadapter"
export CTX='REPLACE_WITH_TEST_CONTEXT'
export NS='agentenv-demo'
export APPNS='agentenv-ssf'
export DRIVER_NS='ssf-fixture'
export WORKER='REPLACE_WITH_NODE_NAME'
export PKI='/absolute/path/to/test-pki'
export API='https://127.0.0.1:9444'
export GUEST_IMAGE='docker.io/library/ubuntu:24.04'
export TEMPLATE_ID='REPLACE_WITH_IMMUTABLE_TEMPLATE_ID'
export TENANT='demo'
umask 077
export LAB
LAB=$(mktemp -d "${TMPDIR:-/tmp}/ssf-realenv.XXXXXX")
mkdir -p "$LAB/requests" "$LAB/manual"
export TEST_CONFIG="$LAB/config.json"
k() { kubectl --context "$CTX" "$@"; }
python3 - <<'PY'
import json,os,pathlib
c=json.loads(pathlib.Path(os.environ['TOOLS'],'config.example.json').read_text())
c.update(url=os.environ['API'],context=os.environ['CTX'],namespace=os.environ['NS'],
         tenant=os.environ['TENANT'],image=os.environ['GUEST_IMAGE'],template=os.environ['TEMPLATE_ID'])
for key,name in [('ca','ca.crt'),('cert','user.crt'),('key','user.key')]:
    c[key]=str(pathlib.Path(os.environ['PKI'],name))
pathlib.Path(os.environ['TEST_CONFIG']).write_text(json.dumps(c,indent=2))
PY
k get node "$WORKER" -o wide
```

将占位符替换为真实值。厂商环境另改 `driver_kind`、两个 StorageClass 和 `DRIVER_NS`。正式性能测试使用实际拉取到的镜像 digest。

首次本地测试可在独立终端开启 control 的 9444 port-forward。测试 control Pod 故障时优先使用稳定的 mTLS 透传地址；若 port-forward 选中的 Pod 被重建，需恢复端口转发后再判断服务恢复，不能将本机转发断开等同系统丢失账本。

### 3.2 自动化入口

```bash
# 先采集基线，失败时先解决采集权限或依赖问题。
python3 "$TOOLS/collect.py" --context "$CTX" --namespace "$NS" \
  --app-namespace "$APPNS" --driver-namespace "$DRIVER_NS" --out "$LAB/before"

# 首次只执行完整数据闭环。
python3 "$TOOLS/suite.py" --config "$TEST_CONFIG" \
  --cases durability --out "$LAB/smoke"

# 默认十个场景；遇到失败/阻塞立即停止后续场景并尝试停止本次已登记实例。
python3 "$TOOLS/suite.py" --config "$TEST_CONFIG" --out "$LAB/default"

# 默认不执行的共享 reader 场景。
python3 "$TOOLS/suite.py" --config "$TEST_CONFIG" \
  --cases shared_readers --out "$LAB/shared-readers"

python3 "$TOOLS/collect.py" --context "$CTX" --namespace "$NS" \
  --app-namespace "$APPNS" --driver-namespace "$DRIVER_NS" \
  --run-report "$LAB/default/report.json" --out "$LAB/after-default"
```

每个 `--out` 必须是新目录，防止覆盖旧证据。`report.json` 持续原子更新；`events.jsonl` 在 RPC 前记录请求意图，之后记录响应、状态和 guest 返回码。即使 POST 结果未知，仍保存原 request_id 和可推导的 operation ID，不能换 ID 假装重试。

退出码非零必须看 FAIL/BLOCKED/未释放记录。默认清理只停止本次已登记 operation，等待真实 Released；404/未知结果保留为 `UNKNOWN_RETAINED`，不清空 SQL/WAL，不隐式删除 PVC。SIGINT 会尝试清理；SIGKILL/终端丢失后按已落盘请求恢复。

原始日志可能仍含 IP、路径或业务参数，按受限运维材料保存。工具对结构化 token/secret 字段脱敏，不承诺任意 Driver 日志的自由文本全部脱敏。

### 3.3 自动化覆盖表

| 编号 / 参数 | 实际场景 | 自动通过标准 | 仍需人工证据 |
| --- | --- | --- | --- |
| S01 `durability` | 编码沙箱删除后重开，继续读取工作目录 | cold RW → template RW → cold RO → template RO，SHA 相同、RO 写 EROFS、每次 Released、PVC UID 保持 | PID/设备和末次释放顺序 |
| S02 `wffc` | 计算位置确定后申请节点相关卷 | 创建后未预绑定/未选点；消费后 Bound/Ready，四轮数据一致 | 选点许可、真实拓扑及未知供卷故障 |
| S03 `multi_volume` | 数据盘和输出盘同时挂载 | 两卷不同内容互不串盘，模板重建各自保持 | 部分 CSI RPC 失败补偿 |
| S04 `idempotency` | 网关重试/客户端重复点击 | 8 个同请求并发只对应一个 ID/instance，改参数 409，释放后旧请求不复活 | 同一实例实际只有一个 VMM |
| S05 `writer_lock` | 两个项目任务争抢写盘 | 第二个明确消费冲突、无 runtime；第一释放后第二成功读原数据 | 跨项目共同账本、实时 SQL 排他 |
| S06 `cancel` | 用户创建后立即取消 | Released 后同请求重试不复活，PVC 保持 | 精确迟到启动窗口，自动化只覆盖实际命中的随机时序 |
| S07 `guard` | 用户绕过入口挂原生 Pod/删除活动卷 | 特定 Guard 拒绝原因；适配删除活动卷返回 pending；实例正常停止 | 排空后后端回收 |
| S08 `validation` | 非法挂载、越权 namespace、保留参数和不支持的恢复 | 400 且原因正确，operation 未登记 | 未映射证书、跨 tenant、FileTree、native 生命周期拒绝 |
| S09 `shared_readers` | 两个分析任务只读同一数据集 | 两 reader 都能读；释放第一个后第二个继续读；最后释放 | 同节点 pin、真实共享 Stage/attachment 和调用次数 |
| S10 `static` | 消费管理员预置的 ext4 卷 | 原 UID、无格式化授权，四轮保留数据；可测试预绑定或未绑定静态 WFFC | 真实候选 PV/拓扑、已有 UUID 未改变 |
| S11 `reclaim` | 用户显式删除卷 | Retain 保留原 PV；显式启用 Delete 后原生 PVC/PV 删除 | 后端实际保留/删除，不能仅看 PV 不见 |
| S12 `performance` | 已就绪工作卷反复启动和并发 | 真实 guest 数据校验，分开 warmup/cold/template 和并发统计 | 足够样本及厂商/硬件基线 |
| S13 `partial_failure` | 第二块盘没有格式化授权，创建失败 | 未 Ready、自动补偿到 Released，第一块盘可再次使用 | Stage/Publish 是否发生和具体逆序；此用例不是“第二个 NodePublish RPC 故障” |
| S14 `uid_mismatch` | 客户端拿着过期 PVC 身份 | 错 UID 不 Ready/不创建 runtime，出现明确冲突，可取消 | 实际同名 PVC 重建，见人工步骤 |

## 4. 人工测试公共工具与证据格式

以下函数仅简化实际 API 命令；人工测试仍按各场景逐步检查，不以运行函数本身判定通过。

```bash
export CLIENT_CERT="$PKI/user.crt"
export CLIENT_KEY="$PKI/user.key"
export RUN_ID="manual-$(date +%Y%m%d-%H%M%S)-$RANDOM"
http() {
  local method="$1" endpoint="$2" body="${3:-}"
  local args=()
  if [[ -n "$body" ]]; then args+=(--data-binary "@$body"); fi
  HTTP_STATUS=$(curl --silent --show-error --cacert "$PKI/ca.crt" \
    --cert "$CLIENT_CERT" --key "$CLIENT_KEY" -X "$method" \
    -H 'Content-Type: application/json' "${args[@]}" \
    -o "$LAB/last-response.json" -w '%{http_code}' "$API$endpoint") || return
  cat "$LAB/last-response.json"
}
client() {
  python3 "$ADAPTER/examples/client.py" --url "$API" --ca "$PKI/ca.crt" \
    --cert "$CLIENT_CERT" --key "$CLIENT_KEY" "$@"
}
wait_state() {
  python3 - "$1" "$2" <<'PY'
import json,os,sys
sys.path.insert(0,os.path.join(os.environ['ADAPTER'],'examples'))
from client import Client
c=Client(os.environ['API'],os.environ['PKI']+'/ca.crt',os.environ['CLIENT_CERT'],os.environ['CLIENT_KEY'])
print(json.dumps(c.wait(sys.argv[1],sys.argv[2],timeout=600),indent=2))
PY
}
new_volume() {
  VNAME="$RUN_ID-$1"
  jq -n --arg ns "$NS" --arg name "$VNAME" --arg sc "$2" \
    --argjson init "${3:-true}" \
    '{namespace:$ns,name:$name,storage_class:$sc,bytes:1073741824,initialize_ext4:$init}' \
    > "$LAB/requests/$VNAME-volume.json"
  http POST /v1/volumes "$LAB/requests/$VNAME-volume.json" > "$LAB/manual/$VNAME-volume.json"
  [[ "$HTTP_STATUS" == 200 ]] || return 1
  VUID=$(jq -er '.metadata.uid' "$LAB/manual/$VNAME-volume.json")
}
make_request() {
  local rid="$1" kind="${2:-cold}" ro="${3:-false}"
  REQUEST="$LAB/requests/$rid.json"
  jq -n --arg rid "$rid" --arg kind "$kind" --arg ns "$NS" --arg pvc "$VNAME" \
    --arg uid "$VUID" --arg image "$GUEST_IMAGE" --arg template "$TEMPLATE_ID" --argjson ro "$ro" \
    '{request_id:$rid,kind:$kind,
      launch:(if $kind=="cold" then {image:$image,cpuCount:2,memoryMB:512,timeout:1800}
              else {templateID:$template,timeout:1800} end),
      mounts:[{namespace:$ns,pvc:$pvc,uid:$uid,id:"data",path:"/data",readonly:$ro}]}' > "$REQUEST"
}
submit() {
  # 请求文件先保存；即便 POST 没有收到响应，也可用原 ID 查询/重试。
  OP=$(python3 - "$TENANT" "$1" <<'PY'
import hashlib,json,sys
rid=json.load(open(sys.argv[2]))['request_id']
print(hashlib.sha256((sys.argv[1]+'\0'+rid).encode()).hexdigest())
PY
)
  http POST /v1/sandboxes "$1" > "$LAB/manual/$OP-create.json"
  [[ "$HTTP_STATUS" == 202 ]]
}
stop_operation() {
  http DELETE "/v1/operations/$1" > "$LAB/manual/$1-delete.json"
  [[ "$HTTP_STATUS" == 202 ]] || return 1
  wait_state "$1" Released > "$LAB/manual/$1-released.json"
}
seed_volume() {
  # 给当前场景创建独立 ext4 测试卷和一份持久数据；结束后无活动 holder。
  new_volume "$1" ssf-block-immediate true || return
  k -n "$NS" wait --for=jsonpath='{.status.phase}'=Bound "pvc/$VNAME" --timeout=180s || return
  make_request "$RUN_ID-$1-seed"
  submit "$REQUEST" || return
  wait_state "$OP" Ready > "$LAB/manual/$OP-ready.json" || return
  client exec "$OP" 'printf "ssf-persisted\n" > /data/probe.txt; sync; sha256sum /data/probe.txt' \
    > "$LAB/manual/$VNAME-hash.txt" || return
  stop_operation "$OP"
}
```

厂商类不叫 `ssf-block-immediate` 时替换 `seed_volume` 中类名。每个场景保存原始请求、响应、时间、前后 Kubernetes/SQL/宿主证据。函数报错就检查原因，不继续把后续结果当作该场景通过。

每个手工场景记录建议采用：

```json
{
  "case": "S01",
  "environment": "E1",
  "status": "NOT_RUN",
  "run_id": "",
  "operation_ids": [],
  "pvc_uids": [],
  "steps": [],
  "evidence_files": [],
  "cleanup_state": "NOT_RUN",
  "limitations": []
}
```

## 5. 业务场景的人工逐步测试

### S01：用户删除开发沙箱后，重新打开原工作目录

1. `seed_volume s01`，确认 Ready、真实 guest SHA 和 Released 全部成功，保留 `VNAME/VUID`。
2. 用模板重建并校验内容：

```bash
make_request "$RUN_ID-s01-template" template false
submit "$REQUEST"
wait_state "$OP" Ready > "$LAB/manual/$OP-ready.json"
client exec "$OP" 'sha256sum /data/probe.txt' > "$LAB/manual/$VNAME-reopened-hash.txt"
diff "$LAB/manual/$VNAME-hash.txt" "$LAB/manual/$VNAME-reopened-hash.txt"
client exec "$OP" 'cat /proc/mounts | grep " /data "'
stop_operation "$OP"
k -n "$NS" get pvc "$VNAME" -o json | jq -e --arg uid "$VUID" '.metadata.uid==$uid'
```

3. 分别用新 request_id 执行 cold/template 的只读创建，核对真实挂载和只读失败原因：

```bash
for kind in cold template; do
  make_request "$RUN_ID-s01-$kind-ro" "$kind" true
  submit "$REQUEST" || break
  wait_state "$OP" Ready > "$LAB/manual/$OP-ready.json" || break
  client exec "$OP" 'sha256sum /data/probe.txt' > "$LAB/manual/$kind-ro-hash.txt" || break
  diff "$LAB/manual/$VNAME-hash.txt" "$LAB/manual/$kind-ro-hash.txt" || break
  client exec "$OP" 'awk '\''$2=="/data" {print $3,$4}'\'' /proc/mounts' || break
  # 上一条输出必须是 ext4，并含 ro 挂载选项。
  if client exec "$OP" 'LC_ALL=C; export LC_ALL; echo forbidden > /data/ro-must-fail' \
       > "$LAB/manual/$kind-ro-write.out" 2> "$LAB/manual/$kind-ro-write.err"; then
    printf '%s\n' 'FAIL: readonly write unexpectedly succeeded'
    break
  fi
  grep -qi 'read-only file system' "$LAB/manual/$kind-ro-write.err" || break
  stop_operation "$OP" || break
  k -n "$NS" get pvc "$VNAME" -o json | jq -e --arg uid "$VUID" '.metadata.uid==$uid' || break
done
```

4. 两轮都必须完整结束；任何 break 均需检查并记录失败/阻塞，停止仍活动的实例。写失败必须是 Read-only file system，不能是连接失败、文件不存在或普通权限拒绝。
5. 核对 Node/Driver 及宿主 FD 证据，最后记录 PASS。

自动化：`--cases durability`。业务意义：用户状态保存在 PVC，不依赖沙箱生命周期。

### S02：第一次创建与节点相关的工作盘（动态 WFFC）

```bash
new_volume s02 ssf-block-wffc true
k -n "$NS" get pvc "$VNAME" -o json > "$LAB/manual/s02-before.json"
make_request "$RUN_ID-s02-first"
submit "$REQUEST"
k -n "$NS" get pvc "$VNAME" -w
# 在另一个终端观察或结束上述只读 watch 后继续：
wait_state "$OP" Ready > "$LAB/manual/s02-ready.json"
k -n "$NS" get pvc "$VNAME" -o json > "$LAB/manual/s02-after.json"
PV=$(jq -er '.spec.volumeName' "$LAB/manual/s02-after.json")
k get pv "$PV" -o json > "$LAB/manual/s02-pv.json"
client exec "$OP" 'echo wffc-real-data > /data/value; sync; cat /data/value'
stop_operation "$OP"
```

逐项检查：创建后 Pending、没有 volumeName/selected-node；首次消费后有固定选点；PV claimRef UID 等于 VUID；PV affinity、真实节点 labels、计算预留节点一致；没有逐沙箱消费者 Pod。不能在消费请求之前等 WFFC Bound。

自动化：`--cases wffc`。供卷暂不可用和结果未知另做 F08/F09。

### S03：训练/构建任务同时挂载输入盘和输出盘

1. 创建两个独立测试卷并保留名字/UID：

```bash
new_volume s03-input ssf-block-immediate true
INPUT_NAME="$VNAME"; INPUT_UID="$VUID"
new_volume s03-output ssf-block-immediate true
OUTPUT_NAME="$VNAME"; OUTPUT_UID="$VUID"
make_request "$RUN_ID-s03-multi"
jq --arg a "$INPUT_NAME" --arg au "$INPUT_UID" --arg b "$OUTPUT_NAME" --arg bu "$OUTPUT_UID" --arg ns "$NS" \
  '.mounts=[{namespace:$ns,pvc:$a,uid:$au,id:"input",path:"/input",readonly:false},
            {namespace:$ns,pvc:$b,uid:$bu,id:"output",path:"/output",readonly:false}]' \
  "$REQUEST" > "$LAB/requests/s03-multi.json"
submit "$LAB/requests/s03-multi.json"
wait_state "$OP" Ready
client exec "$OP" 'echo input-A > /input/value; echo output-B > /output/value; sync; cat /input/value /output/value'
stop_operation "$OP"
```

2. 用新 request_id 模板启动同一组卷，分别读取；交换 JSON 中 mounts 顺序再重建，数据仍必须按 PVC 和 guest path 对应，不能依赖数组顺序串盘。
3. 再用一个只读输入盘、一个可写输出盘重建，输入写失败、输出写成功。模板必须有足够 RW/RO 槽位。
4. 最后停止，两个 PVC 都保留。自动化 `multi_volume` 覆盖两卷 RW 隔离和模板复用；混合 RW/RO 与数组重排为额外人工覆盖。

### S04：上游重试和重复点击创建

1. 创建独立卷，固定 request_id 和请求文件；同时从两个终端或下列命令发相同请求：

```bash
seed_volume s04
make_request "$RUN_ID-s04-retry"
for attempt in $(seq 1 8); do
  client create "$REQUEST" > "$LAB/manual/s04-response-$attempt.json" &
done
wait
jq -r '[.id,.instance]|@tsv' "$LAB"/manual/s04-response-*.json | sort -u
OP=$(jq -er '.id' "$LAB/manual/s04-response-1.json")
wait_state "$OP" Ready > "$LAB/manual/$OP-ready.json"
```

2. 输出只能有一组 id/instance；核对 SQL 只有一条 operation、原生只有一个本次运行实例/VMM。
3. 同 request_id 改 guest path，HTTP 409 且原因是参数冲突；原实例和数据不受影响：

```bash
jq '.mounts[0].path="/different"' "$REQUEST" > "$LAB/requests/s04-conflict.json"
http POST /v1/sandboxes "$LAB/requests/s04-conflict.json" > "$LAB/manual/s04-conflict.json"
[[ "$HTTP_STATUS" == 409 ]]
jq -e '.error | contains("conflicting")' "$LAB/manual/s04-conflict.json"
client exec "$OP" 'cat /data/probe.txt'
stop_operation "$OP"
client create "$REQUEST" > "$LAB/manual/s04-replay.json"
jq -e --arg op "$OP" '.id==$op and .state=="Released"' "$LAB/manual/s04-replay.json"
```

4. 停止并 Released 后再提交原文件，仍返回原 Released 操作，不能创建新 VM。真正的新实例使用新 request_id。并发命令的每份响应都应为有效 JSON；Bash 的单次 `wait` 不替代逐份核对。

自动化：`idempotency`。单凭八个返回 id 相同还不证明底层没启动重复 VMM，须结合实例/进程证据。

### S05：第二个写任务不能破坏正在使用的卷

```bash
seed_volume s05
make_request "$RUN_ID-s05-writer-a"
submit "$REQUEST"; A_OP="$OP"
wait_state "$A_OP" Ready
make_request "$RUN_ID-s05-writer-b"
submit "$REQUEST"; B_OP="$OP"
client inspect "$B_OP"
```

1. 观察至少 15 秒：A 保持正常读写；B 不得 Ready、不得有运行实例；明确出现 `conflicting operation or active resource`。
2. 查询容量预留，确保 B 是因为卷排他受阻，不是没有 CPU/内存。后者记 BLOCKED。
3. `stop_operation "$A_OP"`，然后 `wait_state "$B_OP" Ready`；B 读取 A 的已 sync 数据一致，之后停止 B。
4. 跨项目互斥需额外由另一份接同一 SSF 账本的消费端重复此测试；两个 AgentENV 请求仅证明本路径，不能冒称已验证 CubeSandbox。

自动化：`writer_lock`。任何双 writer 同时 Ready 或 VMM 同时持有写设备都判 FAIL。

### S06：用户立即取消，网络超时后原请求又重试

```bash
seed_volume s06
make_request "$RUN_ID-s06-cancel"
submit "$REQUEST"
stop_operation "$OP"
client create "$REQUEST"
client inspect "$OP"
```

1. 不先等 Ready，立即 DELETE；记录接受停止与 Released 两个时间点。
2. Released 后至少观察 15 秒，重复原请求；instance 不变且不得再次启动。
3. 保留同一请求文件，另做 F06 客户端超时测试；查询原 operation，不能因为 curl 超时就新建 ID 或删 PVC。
4. 若取消时 VM 根本没开始启动，只能记“启动前取消 PASS”。精准的启动竞态/迟到成功必须有 F07 的实际命中证据。

自动化：`cancel`，默认不保证故障时点命中。

### S07：平台管理员误删卷、原生 Pod 尝试挂载

1. 创建新 writer 并等待 Ready，再构造真实 server dry-run 请求：

```bash
seed_volume s07
make_request "$RUN_ID-s07-active"
submit "$REQUEST"
wait_state "$OP" Ready
jq -n --arg ns "$NS" --arg name "$RUN_ID-guard-probe" --arg pvc "$VNAME" \
  '{apiVersion:"v1",kind:"Pod",metadata:{namespace:$ns,name:$name},
    spec:{containers:[{name:"probe",image:"busybox:1.37.0",command:["sleep","60"],
      volumeDevices:[{name:"data",devicePath:"/dev/volume"}]}],
      volumes:[{name:"data",persistentVolumeClaim:{claimName:$pvc}}]}}' \
  > "$LAB/requests/s07-native-pod.json"
k create --dry-run=server -f "$LAB/requests/s07-native-pod.json" \
  > "$LAB/manual/s07-pod.out" 2> "$LAB/manual/s07-pod.err"
k -n "$NS" delete pvc "$VNAME" --dry-run=server \
  > "$LAB/manual/s07-delete.out" 2> "$LAB/manual/s07-delete.err"
k -n "$NS" label pvc "$VNAME" ssf.dev/managed- --dry-run=server \
  > "$LAB/manual/s07-label.out" 2> "$LAB/manual/s07-label.err"
```

2. 最后三条命令应各自非零；逐份检查错误必须分别包含 `SSF-managed volume cannot be consumed by native Pod`、`drain/reclaim permit required`、`cannot remove consumption protection`。不能把连接失败、权限不足或其他准入策略拒绝当作 Guard 通过。
3. 这些 server dry-run 不实际创建 Pod/删除 PVC/去除标签；前后查询原 PVC UID 和活动实例，证明保护对象仍在。
4. 调适配 DELETE 卷接口，预期 409/pending，原 PVC/PV/guest 仍在：

```bash
http DELETE "/v1/volumes/$NS/$VNAME?uid=$VUID&reclaim=Retain"
client exec "$OP" 'cat /data/probe.txt'
```

5. 该请求会记录 drain 意图，即使当前 holder 尚未释放；此后不再用该卷做新消费测试。停止原 operation 后再按 S11 完成 Retain。

自动化：`guard`；错误信息必须来自 Guard/消费账本，RBAC 拒绝或网络失败不能算通过。

### S08：越权和不支持能力必须在副作用前拒绝

1. 创建本场景独立 PVC 和合法请求文件，逐次修改一个字段，保存 HTTP 状态及原因：`autoPause=true`、`autoResume=true`、原生 `volumeMounts/attachedDrives/customExtensionParams`、`/dev/...`、`/data/../...`、未授权 namespace。
2. 每次拒绝后查询推导出的 operation ID，应不存在；数据库不得新增该请求对应预留，CSI 无本次 Attach，原生无本次 VMM。
3. FileTree：管理员在独立 no-provisioner Retain 类下创建 `volumeMode: Filesystem` 的受管 Pending PVC，提交引用它的创建请求，预期 400 不支持，不能尝试 guest FileTree 接入。
4. 暂停/snapshot/fork/expand 并没有适配 API 实现，访问不存在的路径应拒绝；“404”只能证明该 API 未开放。还需在隔离环境通过受信原生测试接口验证带外部设备实例的原生生命周期限制，不能借 guest 代理去调用 native 管理接口。
5. 跨身份：用同一 CA 签发但未列入 principal map 的证书，预期 403 `unmapped client identity`；不带证书应在 TLS 层失败。只测 TLS 连不上不等于 tenant 授权正确。
6. 另配置 tenant B 和其专属 Namespace/证书，B 查询/删除 A 的 operation 应为 404，B 请求 A 的 PVC namespace 应被拒绝。保存 A 仍可读写的证据。

自动化 `validation` 覆盖第 1–2 步中的已列请求；其余逐项人工记录。新增测试身份必须在测试 control principal map 和证书中成对配置，不能把 control 服务证书当租户证书使用。

### S09：多个分析实例共享只读数据，首个退出不影响剩余实例

1. `seed_volume s09` 后，创建 RO A 和 RO B，两者使用新 request_id；确认同节点共享所需计算容量。
2. 两者都 Ready，读出的 SHA 相同；宿主设备 FD 为 O_RDONLY，guest mount 为 ro，写入返回 EROFS。
3. 停止 A 等 Released，B 连续读取和 hash 仍成功；在此时采集 SQL、SSF WAL/日志与 Driver，不能出现破坏 B 的 Unstage/Detach。
4. 停止 B，确认末次 Unpublish/Unstage/Detach 顺序和设备引用最终解除。

自动化 `shared_readers` 验证剩余 guest 可读；实际共享复用和调用次数需底层证据。如果 Driver/access mode 不允许此组合，记明确限制/BLOCKED，不改为可写共享通过。

### S10：已有 ext4 静态卷不能被重新格式化

1. 按 [部署手册静态 PV 章节](13-agentenvadapter-installation-and-deployment.md#132-静态-pv-与预绑定-pvc) 准备独立已有 ext4 的测试后端、受管 Retain PV 和 PVC。预先记录后端 UUID、测试原文件 SHA、PV/PVC UID。
2. 预绑定场景应先 Bound。创建请求直接引用 PVC；不调用 Volume API 补录初始化授权。
3. 冷/模板 RW/RO 各运行一次，原有文件和 UUID 必须保持；新数据落到正确设备。
4. 静态 WFFC 另用新候选 PV 和 PVC：no-provisioner/WFFC、PV Available 无 claimRef、PVC 无 volumeName；确认选中满足拓扑的候选并由原生绑定。
5. 自动化配置 `static_pvc`、`static_mode=prebound|wffc` 后执行：

```bash
python3 "$TOOLS/suite.py" --config "$TEST_CONFIG" --cases static \
  --allow-existing-pvc-write --out "$LAB/static"
```

此脚本会写本次 run 专用文件；不要指向用户真实数据卷。空的已有卷没有初始化授权时应拒绝，不以“没有文件系统”自动 mkfs。

### S11：显式回收 Retain 和 Delete

1. 准备两块本场景独立测试卷，写标记并停止全部 holder；记录原 PV UID、Driver 和 handle。
2. 第一块请求 Retain，重复查询/推进直到 PVC 不存在、原 PV 为 Released 且 reclaimPolicy 为 Retain。用存储后台确认后端仍在；不要立即清 claimRef 重用。
3. 第二块明确请求 Delete，确认 Native provisioner 处理，直到 PVC/PV 完成删除；从厂商后台/API 证明同一个 handle 已删除。loop fixture 可检查对应 `/var/lib/ssf-fixture/block/<handle>` 及元数据文件消失，但不能拿它代替厂商后台证据。
4. 清理链路仍未知时保留报告，不去掉 finalizer、managed label 或 webhook。

```bash
# 仅 Retain；自动创建本次专属测试卷。
python3 "$TOOLS/suite.py" --config "$TEST_CONFIG" --cases reclaim --out "$LAB/reclaim-retain"
# 明确允许删除本次新建的测试后端卷；不会删除 static_pvc。
python3 "$TOOLS/suite.py" --config "$TEST_CONFIG" --cases reclaim \
  --allow-backend-delete --out "$LAB/reclaim-delete"
```

自动化只校验原生资源结果，报告仍要求 `backend_confirmation=MANUAL_REQUIRED`。HTTP 202、`ClaimDeleted` 或 PV NotFound 都不能单独证明后端数据删除完成。

### S12：并发启动的性能和容量

见第 8 节。需要先完成数据正确性与释放测试，再测延迟。超卖测试与吞吐测试分开：前者检查预留上限，后者记录正确请求的延迟和完成率。

### S13：多卷中的第二块盘不可使用，必须补偿整个创建

1. 创建 A：新卷、`initialize_ext4=true`；B：新卷、`initialize_ext4=false`，保持原始空块状态。
2. 提交 `/data0`=A、`/data1`=B 的同一次多卷请求。B 缺少文件系统且无授权，不得 Ready。
3. 保存 Node WAL/Driver 证据，区分“未到 Prepare”“已经 Publish 后 guest 前失败”和“运行时失败”。不能仅看最终 Released 就声称覆盖了所有部分失败点。
4. 原操作应补偿到 Released，两个 PVC 都保留；A 后续可独立消费，B 不得被偷偷格式化。
5. 自动化：`--cases partial_failure`。其设计故障点是 Prepare 后的文件系统授权检查，不是 CSI RPC 注入；精准第二个 NodePublish 失败另见 F13。

### S14：用户缓存了旧 PVC UID

1. 对存在的测试 PVC，在新请求中填写随机错误 UID，预期消费冲突、不 Ready/无 VMM；取消后 Released，当前 PVC 不变。自动化：`uid_mismatch`。
2. 再做真实同名重建：保存旧请求和 UID，先释放全部 holder，再通过适配 API Retain 删除旧 PVC。不要重新创建指向同一个仍被账本固定到旧 UID 的后端 handle。
3. 管理员创建同名但新 UID、独立新后端的受管 PVC。用旧 UID 的新 request_id 提交仍须拒绝；旧 request_id 的 Released tombstone 不得重新启动。
4. 用新的 PVC 名称/UID 走正常创建证明环境可用。适配 Volume API 的同名创建元数据不会自动复用；新卷创建推荐使用新名字。

## 6. 证据采集：SQL、实例和宿主设备

### 6.1 只读 SQL

以下在数据库只读诊断会话执行。用具体实例/operation/PVC UID 过滤，不把整库业务内容导出到公共报告；示例占位符需要替换。

```sql
SELECT id, tenant, instance, state, stopping, sandbox_id, reason, updated
FROM agentenv_adapter_operations WHERE id = 'ADAPTER_OPERATION_ID';

SELECT operation, node, node_uid, boot_id, cpu, memory_mib, state, expires
FROM agentenv_adapter_reservations WHERE state <> 'Released';

SELECT b.id, b.operation_id, b.node, b.sandbox, b.readonly, b.state,
       v.pvc_uid, v.draining
FROM ssf_bindings b JOIN ssf_volumes v ON v.key = b.volume_key
WHERE v.pvc_uid = 'PVC_UID';

SELECT a.volume_key, a.node, a.node_id, a.state
FROM ssf_attachments a JOIN ssf_volumes v ON v.key = a.volume_key
WHERE v.pvc_uid = 'PVC_UID';

SELECT node, sum(cpu), sum(memory_mib), count(*)
FROM agentenv_adapter_reservations WHERE state <> 'Released' GROUP BY node;
```

`agentenv_adapter_operations.id` 和内部 `ssf_operations.id`/reservation.operation 并非相同 ID，不能直接等值 join。通过保存的实例、request、result 中的消费 operation/placement 关联，并用真实表字段确认。查询不到预期行也不自动代表释放成功。

### 6.2 真实运行时和打开设备

1. 保存节点适配记录以及 `/var/lib/agentenv-external/<原生 instance>.json` 中的身份，核对运行实例 UID、PID/starttime、Boot ID、device 列表。原生日志和计划含受信信息，只由管理员访问。
2. 区分 Pod 内 PID 与宿主 PID，用 CRI inspect、`/proc/<hostpid>/status` 的 NSpid 和 PID namespace 建立映射。不能直接拿 Pod 内的 PID 在宿主 kill/查询另一个进程。
3. 将 `device-scan.py` 放到 Worker，以记录的真实 major:minor 扫描；不要从名字猜 `/dev/loop0`。

```bash
# Worker 宿主；DEVICE_MAJOR/DEVICE_MINOR 来自本次可信设备记录。
sudo python3 /path/to/device-scan.py \
  --major "$DEVICE_MAJOR" --minor "$DEVICE_MINOR" --out /tmp/ssf-fds-before.json
# 最后一个 holder Released 后再次采集，文件名必须不同。
sudo python3 /path/to/device-scan.py \
  --major "$DEVICE_MAJOR" --minor "$DEVICE_MINOR" --out /tmp/ssf-fds-after.json
```

运行时应看到对应 VMM 的块设备 FD；readonly 的 access_mode 应为 O_RDONLY（0）。最终不应有本次 VMM 的残留 FD。设备号可能被重用，必须同时核对 handle、节点/Boot、设备路径和时间。

扫描会记录权限不足和扫描中进程消失；它不是 fencing 证明，不能捕捉所有内核引用或消除竞态。必须与 runtime stop 证明、SSF WAL、CSI 返回结果和后端 attachment 一起判断。

### 6.3 释放顺序的判据

同一 operation：先禁止迟到启动、确认具体 VMM 不再使用设备，再 Unpublish；共享 Stage 的最后一个引用才 Unstage；最后一个 holder 才 Detach；最后释放计算预留。

某些组件默认日志只有聚合 RPC 时长，缺少操作/handle 关联。若现有日志无法证明顺序，需为测试环境启用 Driver 官方诊断/trace、审计节点持久状态或引入受控 CSI 测试代理；不能从几条无关联日志推断精确顺序。指标/调用次数不等于设备引用证明。

## 7. 故障场景的人工逐步测试

每个场景单独准备测试卷和实例，采集 before/after。不要在同一批活动实例上叠加多个未知故障；先恢复一个场景并确认资源状态，再执行下一个。

### 7.1 通用进程注入步骤

`host-signal.py` 只在 **Linux 测试 Worker 宿主** 执行。先从指定 Pod/容器得到 containerID，再用该节点的 CRI 确认宿主 PID。例：

```bash
# 管理端：明确指定目标，不使用 pkill/killall，也不默认取集群中第一个同名进程。
TARGET_POD='REPLACE_WITH_EXACT_TEST_POD'
TARGET_CONTAINER='adapter'
k -n "$APPNS" get pod "$TARGET_POD" -o json > "$LAB/manual/fault-target-pod.json"
CONTAINER_ID=$(jq -er --arg c "$TARGET_CONTAINER" \
  '.status.containerStatuses[]|select(.name==$c)|.containerID' "$LAB/manual/fault-target-pod.json")
printf '%s\n' "$CONTAINER_ID"
```

在对应 Worker 执行，以下 `CRI_ID` 去掉 `containerd://` 等前缀：

```bash
CRI_ID='REPLACE_WITH_EXACT_CONTAINER_ID_WITHOUT_SCHEME'
TARGET_PID=$(sudo crictl inspect "$CRI_ID" | jq -er '.info.pid')
# 不同 CRI 的 inspect 字段可能不同；字段不存在就停止，按运行时确认，不能猜 PID。
TARGET_EXE=$(sudo readlink "/proc/$TARGET_PID/exe")
TARGET_START=$(sudo python3 - "$TARGET_PID" <<'PY'
import pathlib,sys
print(pathlib.Path('/proc',sys.argv[1],'stat').read_text().rsplit(')',1)[1].split()[19])
PY
)
TEST_BOOT=$(cat /proc/sys/kernel/random/boot_id)
sudo python3 /path/to/host-signal.py --pid "$TARGET_PID" --start-ticks "$TARGET_START" \
  --exe "$TARGET_EXE" --boot-id "$TEST_BOOT" --signal KILL --out /tmp/ssf-fault-kill.json
```

脚本在发信号前持久化 intent、打开 pidfd 并复核身份，不向 PID 1 或 Firecracker 发信号；允许目标限于选定的测试服务进程。改为 `--signal STOP --hold-seconds 5` 可短暂停顿，脚本会通过同一 pidfd 自动 CONT，最长 10 秒。liveness 也可能重启被暂停服务，需记录实际发生的是“继续运行”还是“容器被替换”。

`INJECTION_COMPLETED` 只证明注入动作执行，不证明恢复成功。原生进程位于共享 PID namespace 时，**不要使用容器内 `kill 1`**；它可能指向 pause/init。若注入工具被 SIGKILL 导致无法 finally 恢复 STOP，管理员应按保存的 PID/start/Boot 身份核对后恢复进程或按容器重启流程处理。

### F01：控制服务在创建中崩溃

1. 准备一个新的创建请求并提交；从 control 日志确定处理该 operation 的具体副本。
2. 记录此时的 request、operation、instance、账本 steps/attachment、Node 状态；选在请求已经登记但尚未 Ready 的时点，按 7.1 KILL 该 control 进程。
3. 观察 kubelet 重启和另一 control 副本恢复；必要时恢复本机 port-forward。用**同一请求文件**重试。
4. 应仍是同一 operation/instance；真实 VMM 最多一个。未知 Attach 不得换 node_id，已有卷/预留不得凭超时释放。
5. 达到 Ready 后写读并停止；若中途取消则最终 Released。采集原副本 previous 日志和恢复副本日志。

如果只执行 `rollout restart` 或创建已经 Ready，分别记录“优雅滚动恢复”或“Ready 后重启”，不算命中创建中的崩溃场景。

### F02：节点适配器崩溃，原生 VM 继续运行

1. S01 准备 Ready VM，写入并 sync，记录 Pod UID/PID namespace、原生 sandbox_id、VMM PID/start 和设备。
2. 只 KILL 该 Pod 的 `adapter` 容器主进程；不删除整个 Pod，不杀 AgentENV/Firecracker。
3. 重启后 Pod UID/PID namespace 应保持；适配器从同一 bbolt 恢复。读原数据成功，引用不能被无条件释放，运行实例不能变成第二个 VM。
4. DELETE 并等待 Released，核对持久日志、末次引用和孤儿扫描。

若整个 Pod 被替换，转按 F10 记录，不能仍算 F02。

### F03：AgentENV 原生 owner 崩溃，清理孤儿 VMM

1. 准备 Ready 实例，写入/sync，记录 owner 与 VMM 的不同 PID/start。
2. 只 KILL `/server` 对应的原生主进程，保留节点适配器和共享 PID namespace。
3. 原生 owner 退出后，节点适配器应识别其已登记实例；仍有 VMM FD 时不得先 Unpublish/Detach。观察按 pidfd 停止孤儿、取得停止证据、再释放。
4. 原 operation 应进入停止/Released，不自动恢复 VM 内存或生成新实例。
5. 用新 request_id 重建同 PVC，验证已 sync 数据保持。owner 非正常退出时 ext4 可能需日志恢复；先走获授权 RW 启动，不用 RO 强行绕过恢复。

保留故障前后 scan、native journal、adapter/SSF/CSI 日志。仅看到原生 Pod Running 不是成功标准。

### F04：SSF Node 在 Prepare/Release 中崩溃

1. 准备新请求，在节点日志/WAL 可证实 Preparing、Stage/Publish 或 Release 已有 intent 时，KILL 对应 `ssf-node` 进程，保留宿主 WAL。
2. 重启同配置/Node UID/Boot 身份，重复原请求。未知 RPC 继续按原步骤恢复；不能分配第二个不受控设备或丢失引用。
3. 准备阶段恢复后 guest 数据面成功；释放阶段恢复后 Unpublish/末次 Unstage/Detach 完成。
4. 扫描原目标和设备，无本次孤儿引用。CSI 幂等重复调用可以发生，不要求每个方法只调用一次。

若没有观测到具体时点，将记录降为“SSF Node 普通进程重启”，精准点位记 NOT_HIT。

### F05：停止通知丢失，但真实停止日志已经落盘

此场景只阻断 **Ready 实例的停止通知**，不阻断启动授权阶段。使用 AgentENV 容器中的 iptables；若环境没有命令/权限，记 BLOCKED，不能用删除整个 Pod 替代。

```bash
# 管理端：AENV_POD 为本实例所在的明确 Pod。
AENV_POD='REPLACE_WITH_EXACT_AENV_POD'
RULE_TAG="ssf-f05-$RUN_ID"
k -n "$APPNS" exec "$AENV_POD" -c agentenv -- \
  iptables -I OUTPUT 1 -o lo -p tcp --dport 9445 -m comment --comment "$RULE_TAG" -j REJECT
# Ready 实例已经存在，随后发起正常删除。
http DELETE "/v1/operations/$OP" > "$LAB/manual/f05-delete.json"
client inspect "$OP"
# 保存日志/原生 stopped journal 后，恢复准确的测试规则。
k -n "$APPNS" exec "$AENV_POD" -c agentenv -- \
  iptables -D OUTPUT -o lo -p tcp --dport 9445 -m comment --comment "$RULE_TAG" -j REJECT
wait_state "$OP" Released
```

过程中的 runtime 状态和节点扫描必须证明 VMM 真正停止。适配器可通过共享的持久原生日志恢复停止证据；不能以“Hook 请求返回失败”或者“通知已发送”代替。测试结束核对规则已经删除；若操作中断，由管理员按同一 RULE_TAG 恢复。

### F06：客户端创建超时，不知道服务端是否接受

1. 保存合法请求文件和确定 request_id，用极短超时发起真实 POST：

```bash
curl --max-time 0.02 --cacert "$PKI/ca.crt" --cert "$CLIENT_CERT" --key "$CLIENT_KEY" \
  -H 'Content-Type: application/json' --data-binary "@$REQUEST" "$API/v1/sandboxes" \
  > "$LAB/manual/f06-short-response.txt"
# 不论超时还是收到响应，都只重复同一文件。
submit "$REQUEST"
wait_state "$OP" Ready
```

2. 查询日志/数据库确定第一次请求到底有没有被接受，记录为已命中“响应丢失”、或“请求根本没到达”。两者不能混为同一种故障。
3. 重试后唯一 operation/instance，只有一个 VMM；完成数据写读并停止。
4. 再做“超时后取消”，同 ID 删除并等待 Released；观察原请求迟到是否仍被 tombstone 阻止。

此用例不证明 CSI ControllerPublish 的 RPC 响应丢失，后者见 F08。

### F07：创建已经开始启动 VM，同时收到取消

1. 准备请求并实时观察该 operation 的节点记录。管理员可使用受信 control 证书访问固定 node Service 的只读 `/v1/operations/{id}`；普通 tenant 证书不应能访问。
2. 当记录已 `dispatched=true`/Launching 且具体 native instance 已登记，立即 DELETE；保存两个请求的时间、节点记录和原生事件 sequence。
3. 重复这一试验，直到确实命中“启动授权/启动副作用已发生、停止意图随后到达”的窗口。先停止再获授权的场景单独记启动前取消。
4. 应拒绝后续迟到启动或停止已经启动的具体 VMM，确认退出后才释放，Released 后旧事件不能使状态回到 Ready。
5. 检查有无第二个 instance、孤儿进程和未释放附件。

当前没有外置可配置的“启动阶段屏障”开关。纯时间 sleep/随机取消未命中时记 NOT_HIT；需要严格可重复时，应在独立测试部署中增加 Runtime/网络测试代理屏障，或采用项目正式支持的故障钩子，不能改生产 journal 来伪造阶段。

### F08：后端 Attach 已成功，但调用方没有拿到结果

前置：能独立只读查询后端 attachment，并有可控的 CSI gRPC 测试代理/厂商故障设施。**当前仓库测试 Driver 没有按 RPC 配置失败/延迟的开关，本次也没有把该设施伪装成已实现。**

人工步骤：

1. 为本次独立测试卷在代理上匹配 ControllerPublishVolume 的 driver/handle/node_id；仅拦截响应，不阻止请求到达真实 Driver。
2. 发起创建，等后端确认该 handle 已 attach 到真实 node_id 后丢弃/延迟这一次响应，保存代理记录及后端记录。
3. 让调用方超时，再恢复代理；核对 SSF 保留原 attachment/node pin 和未知步骤，不释放权限、不换节点再 Attach。
4. 同 operation 重试幂等收敛；Ready 后 guest 可用，或取消后在停止证明成立时最终 Detach。
5. 验证后端最终只有预期 attachment，无悬挂/跨节点并发。

只有“关掉 Gateway”或“暂停 Driver 进程”通常覆盖不可用/延迟，不一定命中**成功但响应丢失**。没有上述观测和设施时，此用例必须保持 BLOCKED/NOT_RUN，可先用 `host-signal.py --signal STOP` 测短暂不可用并另立记录。

### F09：WFFC 供卷停顿，取消不能盲目换节点

1. 新建 WFFC PVC，保持 Pending。记录 CPU 预留空闲和原 Node UID/Boot ID。
2. 在隔离测试环境短暂停顿具体 provisioner 进程，例如 `host-signal.py --signal STOP --hold-seconds 10`。若多副本 leader 会切换，需记录实际 leader 和请求是否仍执行，不能假定全部供卷被暂停。
3. 发起创建，观察预留、selected-node/选点许可和是否产生 PV；在原操作上取消。
4. provisioner 恢复后，若卷晚到 Bound，原操作应按固定选点和 UID 完成补偿；不得另选一节点创建第二个卷。
5. 核对最终只存在一份后端身份、无遗留 holder、删除沙箱仍保留 PVC。

只在供卷前暂停是“供卷暂不可用”测试；要证明 CreateVolume 结果未知/迟到，需要额外原生 provisioner/Driver 屏障和后端记录，未命中不能算通过。

### F10：整个 AgentENV Pod 被替换，宿主没有重启

1. 准备 Ready VM；保存原 Pod UID、PID namespace、Boot ID、Node UID 和 VMM 身份。
2. **仅在本隔离场景**删除该节点 AgentENV Pod，等待其控制器重建，不删 HostPath/WAL：

```bash
k -n "$APPNS" delete pod "$AENV_POD" --wait=true --timeout=180s
k -n "$APPNS" get pods -o wide
client inspect "$OP"
```

3. 预期新 Pod UID/PID namespace 变化、宿主 Boot ID 不变。框架应保留无法证明停止的引用并给出诊断；不能以新 Pod 里看不见旧 PID 就报告安全释放。
4. 通过宿主进程/设备和受控 fencing 取得确切停止证据，按 F11 恢复。此处“保留占用”是预期保护行为，不通过手工改 SQL 或删 WAL 强行获得 PASS。

正常升级应先排空，此用例故意违反正常升级顺序来检验 fail-closed 边界。

### F11：宿主重启 / Node 对象身份变化

**宿主重启子场景：**

1. 保存全部当前实例和卷、Node UID、Boot ID、数据库和节点日志。确保测试宿主上没有不在本次范围的工作负载。
2. 在 Worker 管理通道执行受控重启，例如 `sudo systemctl reboot`；不要把 `kubectl delete node` 当作重启。
3. 节点回来后，Node UID 应相同，Boot ID 改变；等待数据库/CSI/Node/适配恢复。旧 VM 内存不自动恢复，旧引用只能基于可信 reboot fencing 清理。
4. 旧 operation 最终停止/Released；新 request_id 重建同 PVC，已 sync 数据保持，可能执行正常 ext4 日志恢复。

**Node 对象重建子场景：**使用另一套可销毁集群，保留旧 UID 记录后重建同名字 Node，确认新 UID 不同。旧操作必须身份冲突并保留占用，不能因为名字相同接受旧证明。跨 UID 自动迁移/强制解锁不属于当前能力；完成诊断即记“拒绝旧身份 PASS”，自动恢复保持未支持，不要求伪造 Released。该场景的遗留清理由平台按真实节点/存储证据制定，不能和普通重启共用快捷清理命令。

### F12：共享数据库暂不可用

1. 用一个已 Ready 实例开始有限次数、带序号且 sync 的写入任务，另外准备新创建请求。保存已提交的账本和实例映射。
2. 隔离环境由 DBA/网络层暂时中断 control、Guard、Gateway 对测试 DB 的连接，保留数据库磁盘。不要清库，也不要在业务库运行故障脚本。
3. 新消费不得绕过授权；PVC/PV 保护操作在 Guard 无法验证时应拒绝。已有 VM 的数据面不依赖 DB 转发，不应因为 DB 超时立即卸载设备。
4. 恢复连接后重试原 request_id；账本幂等收敛，无双 writer 或丢失引用。恢复代理后核对 guest 已写数据，再正常停止。
5. 记录 RTO、未知状态持续时间及是否需要人工干预。测试 PostgreSQL 进程重启只是数据库暂不可用的一种，不能替代分区/提交结果未知测试。

### F13：精准 CSI 错误、旧证据和计划重放

这组是高级验收，不包含在默认自动化中。每项必须具有真实可识别的注入点：

| 子项 | 人工步骤与判据 |
| --- | --- |
| 第二个 NodePublish 失败 | 测试代理匹配第二个 binding 的真实 RPC，在第一个成功后对第二个返回指定错误；创建不 Ready，按真实成功/未知状态补偿，首卷引用不泄漏。S13 的文件系统故障不替代此项 |
| NodePublish 成功响应丢失 | 宿主确认 target 已 publish 后阻断响应；WAL 保留未知，原计划重试验证实际 target/设备，不能重复分配或直接删除 |
| Detach 成功响应丢失 | 后端确认 Detach，账本尚未确认时断响应；重复相同 RPC 收敛，不产生假释放权限 |
| 旧 runtime event | 保存旧 instance/sequence 的事件，在新状态后通过受信测试入口重放；身份/序号不匹配被拒绝，不回退 Ready/Stopped/Released |
| 篡改/过期签名计划 | 使用受限测试客户端向 node 提交修改一字节或过期计划；拒绝发生在 Prepare 副作用前，WAL/Driver 无对应新操作 |
| 节点启动身份不符 | 对旧 Boot ID/Node UID 的计划/证明做重放，拒绝并保留旧权威，不能依据字符串相同复用 |

执行者需记录测试代理/客户端版本和注入配置。现有生产 API 不对租户开放这些管理入口；不能把开放端口或去掉鉴权作为测试准备。没有对应设施/证据的子项留 NOT_RUN，不能用本机 mock 契约测试替代。

## 8. 多节点、容量、性能和持续运行

### 8.1 拓扑与计算联合选择（E3）

1. 真实节点 A/B 位于不同可验证存储拓扑域；记录 Node labels、CSINode ID、后端可用域，不通过伪造 zone label 让不兼容卷看似兼容。
2. Immediate：提前创建 A 域卷。让仅 B 有足够计算余量，提交消费；应保持未就绪并明确没有联合可行点，不能在 B 盲目 attach。释放 A 余量后原操作可在合法节点推进。
3. WFFC：新建未绑定 PVC，在 B 有计算余量且存储可供给时消费；原生供卷结果必须真实满足 B 拓扑，最后 PV affinity/计算预留/运行节点一致。
4. 静态 WFFC：准备两个合法候选，一个拓扑不匹配、另一个匹配；按真正容量/模式/标签/拓扑选择匹配 PV，不能因为列表中第一个候选不匹配就放弃所有候选。
5. 供卷/绑定结果未知期间中断节点可见性；必须保留原选点及引用，不能自动换节点重建未知后端。
6. 验证已经 attach 到 A 的单节点卷不能被第二个节点的独立实例并发使用。故障后跨节点迁移必须先有真实 fencing 和后端 detach 证据；不是租约过期后自动放行。

主线的本地 fixture 不能用来宣称多节点通过。模板仓库也必须在目标节点真实可见。

### 8.2 计算容量压力

在无其他任务的专用池记录 capacity，例如 4 CPU/4096 MiB，每 VM 2 CPU/512 MiB：

1. 使用两个独立 PVC 并行创建 A/B，等待 Ready；只读 SQL 的所有非 Released 预留总 CPU 不得超过 4。
2. 第三个独立 PVC 请求 C 应等待/失败为“无兼容容量”，而不是越过容量启动；它与 S05 writer 互斥是不同测试。
3. 停止 A 并等 Released，C 才有机会取得容量。仅把 A 租约时间调过期不能释放其数据面/计算占用，不修改数据库强造过期。
4. 重启 control 后重复查询，容量账本不丢失、不重复计数。失败与取消操作最终释放其真实预留。

可用 `performance` 发并发压力，但精确超卖上限需要本节 SQL 采样和运行实例数，不能仅用 HTTP 成功率推断。

### 8.3 性能测试

先少量运行验证脚本，再按预算增加 `performance_runs`。自动化每个 run 建一个新卷并 warmup 初始化，然后测已格式化卷的 cold 和 template 启动，且每次都做数据校验与停止：

```bash
python3 - <<'PY'
import json,os,pathlib
p=pathlib.Path(os.environ['TEST_CONFIG']); c=json.loads(p.read_text())
c['performance_runs']=100
c['concurrency']=2
p.write_text(json.dumps(c,indent=2))
PY
python3 "$TOOLS/suite.py" --config "$TEST_CONFIG" --cases performance --out "$LAB/perf-c2"
```

分别执行并发 1、2 及容量允许的更高值，使用独立输出目录。100 次会保留约 100 个 1 GiB PVC；预留空间和清理窗口，首次验证可先 5–20 次。关键分组：

| 分组 | 含义 |
| --- | --- |
| `warmup-initialize` | 初次空白扫描/初始化；单独记录，不混入复用分位数 |
| `measured-reuse / cold` | 已就绪卷，冷启动 guest |
| `measured-reuse / template` | 相同已就绪卷，模板启动 |
| `shared-reuse / ro` | 共享 reader 场景；与独占 RW 分开 |
| 动态 WFFC 首次 | 单独报告供卷/绑定，不与提前 Bound 卷混算 |

`overall_ms` 包含真实客户端轮询、网络和持久事件日志 fsync 开销，是**这个验收客户端观察到的创建延迟**。`poll_seconds` 默认 250 ms，p50/p95/p99 有观察量化，不能称为纯 VM 启动时间。server 的 `timings_ms` 是阶段累计值，可能包含重试和重叠，不能简单相减得到精确 VMM 耗时。

需要细分 Stage/Publish 时，采集 SSF/CSI 的对应指标/trace，并保留操作关联；当前高层报表只有其已有阶段字段，不虚构缺失阶段的分位数。HTTP 连接是否复用、TLS 握手、日志 fsync、镜像/模板缓存和空白扫描均写入报告。若要做极限性能基准，应另外使用长连接负载客户端并校准观察开销，同时保留本方案的数据正确性检查。

至少记录样本数、完成率、失败/超时数、CPU/内存/IO/数据库延迟、Driver/后端版本、节点和网络拓扑。小样本 p99 仅供观察，不作 SLA。

### 8.4 持续运行和资源增长

1. 在固定容量下连续执行创建 → 数据校验 → Released 循环，先 1 小时，再按验收目标延长至 8/24 小时。
2. 每隔固定时间采集进程数、FD、mount、loop/ublk、连接数、数据库体积和 WAL。记录实验创建的卷和正常保留的 tombstone 数，区分预期历史增长与泄漏。
3. 抽样重启进程并检查数据、幂等及释放。不要并发执行未设计好的主机重启和数据库分区。
4. 结束后无本次活动 VM、无非 Released 预留/holder，残留 Retain PVC/后端有完整资产清单。任何未知引用作为未解决项报告。

## 9. 回归、清理与最终交付

### 9.1 与现有测试组合

```bash
# 真实 Linux 测试主机：临时 PostgreSQL、真实挂载和镜像构建。
bash "$ADAPTER/tests/preflight.sh"
bash "$ADAPTER/tests/linux-integration.sh"
# 关闭外部卷扩展的原生路径回归，保存完整输出和 Skip。
(cd ../AgentENV && cargo check --locked -p agentenv)
(cd ../AgentENV && cargo test --locked -p agentenv --lib)
```

进一步运行 AgentENV 原有不带外部卷的冷/模板启动、pause/snapshot/fork 流程，证明扩展关闭时原路径不受影响；其具体上游测试入口见 [AgentENV e2e 套件](../AgentENV/scripts/tests/e2e/suites/05_template_lifecycle.sh)。原有路径回归和本适配“不支持”的负面测试分别记录，不能互相替代。

### 9.2 清理与异常中断

1. 从本次 `report.json`/请求文件列出 operation 和 PVC UID，逐个查询；未知 POST 用原 tenant+request_id 推导的 ID 和原请求幂等恢复，不创建新 ID 掩盖旧请求。
2. 对实际已登记操作 DELETE 并等 Released；对于查询 404 的未知请求，核对服务端持久记录及是否存在迟到调用。404 不是证明从未发生副作用。
3. 保存必要 evidence 后，只对本次新建测试 PVC 通过适配 DELETE 申请 Retain/Delete；固定原 UID。现有 static_pvc 默认由管理员按数据策略保留。
4. 确认全部 iptables/代理故障规则、暂停进程、改过的副本数/配置已经恢复；临时 Node/拓扑试验的遗留单独交接。
5. 不删除 SQL 行、节点 WAL、原生日志、挂载路径或 whole Namespace 来“清场”；数据库与后端未知状态需先核对。

自动化不会在正常测试结束时默默删除保留数据。显式 `reclaim` 用例例外，它只操作本次为该用例创建的卷；Delete 分支还需 `--allow-backend-delete`。

### 9.3 报告目录和门槛

建议归档：

```text
RUN/
  environment/       硬件、内核、K8s、CRI、Driver、镜像 digest、源码/补丁 SHA
  before/ after/     collect.py 输出及 index.json
  suite-*/           report.json、events.jsonl
  manual/            Sxx/Fxx 分步记录、HTTP/SQL、原生日志、PID/FD/后端证据
  performance/       分组样本、分位数、完成率、系统资源曲线
  cleanup/           Released 清单、保留 PVC/后端资产、未知引用和恢复交接
  acceptance.json    各场景 PASS/FAIL/BLOCKED/NOT_RUN/NOT_HIT 和评审结论
```

上线候选至少完成 S01–S14 中适用场景、身份隔离、Guard、真实静态/动态模式、F01–F05/F10–F12、设备/末次释放证据和原生回归；精确 RPC/竞态、厂商和多节点未验证的部分必须明确限制使用范围。出现数据损坏、双 writer、越权、无 fencing 释放、未排空回收或不可解释孤儿设备，不能用性能结果抵消。

一份“完整测试方案”允许部分项目暂时未执行；一份“完整通过报告”不能省略未执行项。最终是否进入生产由实际环境的结果和使用范围决定，不由工具自动设置认证标记。

### 9.4 本次交付状态

新增真实场景 runner、证据采集、Linux 设备扫描及定点进程注入工具，并提供逐场景人工流程。源码和命令检查记录见 [测试方案交付检查](../SSF/adapter/agentenvadapter/validation/realenv-test-plan-checks.json)。

**未在当前主机执行上述真实环境测试、任何故障注入或集群修改。** 本机辅助单元测试只检查测试工具自身，不代表 SSF/AgentENV/Firecracker 通过。K8S CSI 架构参考原文保持不变。
