# AgentENV 原生插件测试与验收

> 2026-10-21 自动卷准备增量：实现、部署、验证结果及未完成边界统一见 [22](22-volume-preparation-implementation.md)；直接 PVC 两步场景见 [20](20-kubernetes-pvc-agentenv-mount-validation.md)。本文件原生命周期说明继续适用，旧未启用 profile 的卷策略不代表新 profile 规则。

本文件描述可执行验证和判据；实际结果见 [18](18-native-integration-implementation-status.md)。fixture、fake Kubernetes、交叉类型检查均不代表真实厂商 CSI/KVM 成功。

## 本机验证

在 `SSF` 目录执行：

```sh
python3 scripts/validate_native_foundation.py --output validation/native-plugin-progress
python3 scripts/native_database_test.py --postgres-bin /path/to/postgresql/bin
python3 scripts/validate_native_deployment.py --kustomize /path/to/kustomize
python3 scripts/check_native_openapi.py --java /path/to/java --jar /path/to/openapi-generator-7.22.0.jar --rustfmt /path/to/rustfmt
python3 scripts/check_native_rust_modules.py --cargo /path/to/cargo --offline
python3 scripts/agentenv_patch.py --output adapter/agentenvadapter/patches/agentenv-native-progress.patch
python3 adapter/agentenvadapter/scripts/check-artifacts.py
```

foundation 执行 SSF Go 单测/race/vet、Linux 命令构建、protobuf 生成一致性、公开 OpenAPI 向后兼容、原 Go 普通/SSF 构建和测试；逐项保留 skip。数据库脚本创建可销毁私有本机实例，执行真实 PostgreSQL 事务/并发/恢复测试；Kubernetes 客户端仍使用 fake。Linux `check_agentenv_linux.sh` 执行原生 Rust all-target 类型检查与相关单测；macOS `check_agentenv_cross.py` 仅检查 Linux 目标，不链接或运行完整 Rust 测试。便携模块测试确实运行 freeze/artifacts/transition，但不替代 VMM。

## 真机执行顺序

1. `native_preflight.py --context CONTEXT --namespace NS --storage-class SC --snapshot-class SNAP --output preflight.json`；standalone 加 `--mode standalone`。检查 Linux、KVM、工具、CSINode、SC/SnapshotClass、原生工作负载。预检不能证明数据语义。
2. `native_compatibility_e2e.py` 分别针对 provider disabled 和 enabled-no-ssf 运行，覆盖原 cold、v1/v2、list/get/delete、timeout、自动暂停/连接恢复、模板和 envd 流代理。custom extension、HTTP/WS、template build、P2P 通过 `--scenarios` 接入该环境原 SDK/服务的 argv 场景；未提供会逐项记未执行，完整矩阵为 false。须附部署配置证据证明实际 provider 模式。
3. `native_lifecycle_e2e.py --execute` 通过原 Gateway 和 guest envd 检查冷启动、数据持久、活动/暂停排他 writer、恢复同 ID、connect 恢复、离线扩容及缩容拒绝、快照源继续运行、Fork 两个独立 writer、模板省略/空数组/替换。恢复内存中的后台进程持有真实 FD，验证旧挂载隐藏后 FD 仍读旧内容。`--mixed` 同时覆盖 legacy；`--import-pvc` 固定专用已初始化 PVC namespace/name/uid，禁止用于用户生产数据。
4. 对动态/静态 × Immediate/WFFC 分别运行；静态导入需为每次用例预先准备匹配 Class、topology、accessModes、Block、capacity 的独立 PVC/PV。只读共享另用专用只读数据卷；测试写入失败、双读成功、快照/删除引用保护。共享框架 DB/单测已覆盖这些准入，但实际后端组合必须逐项执行。
5. 执行故障场景和回收检查，采集 ledger/WAL/Pod/CSI/VMM/guest 证据；完成后按报告里的已知 native ID 清理，不自动重试结果未知的 create。

## 故障注入工具

`native_fault_e2e.py` 读取版本 1 JSON：`cases` 每项包含 `name`、`stage`、`workload`、`recover`、`assertions`，后三项是非空 argv 数组列表；另有 `inject` argv 列表或 `csi_fault`。无 `--execute` 只生成未执行报告；必须明确 context、namespace。命令是受信任本地测试代码，不经过 shell 插值，不保存可能含密钥的 stdout/argv。

CSI fixture 通过 `render_fixture.py --fault-file /data/fault.json` 显式开启，生产 SSF/CSI 没有故障开关。工具检查容器确为 `ssf-test-driver` 且明确配置 fault-file，才经 kubectl exec 写入计划。规则字段为完整 CSI method、before/after、count、可选 delay_ms 和 code；after 先执行真实 fixture 副作用再返回错误。计数 fsync 跨重启持久，事件不含请求体/凭据。每条规则实际触发后才算注入成功；工具关闭注入后执行恢复及所有断言，失败即停止叠加下一故障。

示意场景（命令替换为该测试环境脚本，不能用 `true` 冒充断言）：

```json
{"version":1,"cases":[{"name":"unknown-attach","stage":"attach","csi_fault":[{"method":"/csi.v1.Controller/ControllerPublishVolume","phase":"after","count":1,"code":"Unavailable"}],"workload":[["./cases/create-once"]],"recover":[["./cases/reconcile-original-request"]],"assertions":[["./cases/assert-one-vmm-and-writer"],["./cases/assert-ledger-wal-and-devices"]]}]}
```

交付的 `native_invariants.py --psql /path/to/psql --output invariants.json` 使用 `SSF_DATABASE_URL` 和只读 repeatable-read 事务检查排他 owner 冲突、并发 run、维护屏障冲突与回收活引用；只输出数量，不输出 DSN/行身份。本机已用正常库和故意双 writer fixture 检查成功/失败出口。该工具不能证明实际 VMM 数量、设备 FD 或 guest 数据，仍需对应物理证据断言。

场景应覆盖 reserve、bind、attach、dispatch、spawn、ready-commit、pause、freeze、publish、release。进程/网络/DB 中断由 `inject` 的显式测试脚本执行，恢复脚本必须重查原 request/native ID，不重试新 create。报告总是列出未验证 stage，完整矩阵只有全部 stage 实际成功才为 true。

| 断点 | 必须验证的结果 |
| --- | --- |
| 预留/绑定/Attach | 单 owner、无重复 PVC、无永久丢失计算预留；未知 Attach 查询恢复 |
| 派发/spawn/Ready/Commit | 同一 request 幂等；无重复 VM；未知 stop 保留原 handle；Ready 不能伪造 |
| pause/恢复 epoch | pause hold 排斥第二 writer；旧 run 事件不提交新 run；旧 VM 停止后接续 |
| freeze/联合发布 | 所有卷同屏障，失败 thaw；不公开部分 capture；原仓库与 CSI pin 不丢失 |
| release/回收 | stopped 证据先于卸载和 detach；pin/UseHold 阻止删除；迟到 Snapshot/Content 被 tombstone 接续 |
| 节点/插件重启 | WAL/outbox 重放，设备 UID/UUID/RO 校验；未知结果不误格式化、不强拆 |

## 证据与清理

`native_evidence.py` 只读采集指定 context 的资源和有限容器日志，不查询 Secret；日志仍按运维敏感材料处理。`native_cleanup.py --report ...` 默认只打印计划，仅删除报告里已知 ID；`--execute` 才执行，`--templates`/`--volumes` 另行显式选择，导入卷不删。未知创建需人工按 request/native ID 对账，不能当成不存在。验收必须区分 fixture 与 vendor、passed/failed/not_executed；完整矩阵的未执行项目不能被总状态 passed 掩盖。
