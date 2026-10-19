# 真实环境场景测试工具

完整前置条件、场景和逐步人工操作见 [独立测试方案](../../../../../design/14-agentenvadapter-real-environment-test-plan.md)。首次部署见 [安装手册](../../../../../design/13-agentenvadapter-installation-and-deployment.md)。

这些工具只使用 Python 标准库；`suite.py` 复用本项目真实 mTLS/guest 客户端，并调用指定 context 的 kubectl。需要已部署的 Kubernetes/PostgreSQL/CSI/AgentENV/Firecracker，不能在 macOS 上模拟端到端成功。

```bash
# 从 SSF 根目录；先复制并填写 config.example.json。
python3 adapter/agentenvadapter/tests/realenv/suite.py \
  --config /absolute/path/config.json --cases durability --out /tmp/ssf-realenv-new-run

# 仅离线工具检查，不连接真实环境、不代表 VM 通过。
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover \
  -s adapter/agentenvadapter/tests/realenv -p 'test_*.py' -v
```

- `suite.py --help`：14 个可选场景；默认十项，失败/阻塞停止后续场景。
- `collect.py --help`：只读 Kubernetes 和容器日志证据，输出部分失败详情，不读取 Secret 内容。
- `device-scan.py --help`：真实 Linux 宿主块 FD 观察；扫描结果不单独作为 fencing。
- `host-signal.py --help`：按明确身份/pidfd 注入测试服务进程故障，STOP 自动恢复；必须按手册在隔离 Worker 使用。

`report.json` 和 `events.jsonl` 持续保存原请求、operation/instance、卷 UID、真实返回码及统计。每次输出目录必须新建。默认只停止本次运行登记的实例，不删除 PVC；未知结果保持记录。`static` 需指定专用静态 PVC 并加 `--allow-existing-pvc-write`；`reclaim` 的 Delete 分支需 `--allow-backend-delete`，只删除该用例新建卷。

`PASS_SELECTED_CASES` 不表示所有故障/底层释放顺序/后端删除/厂商 Driver 均通过。精确 RPC 响应丢失和逐 RPC 故障需要额外测试代理或厂商设施，当前 fixture 没有虚构的故障配置开关。不要把本目录的离线单元测试或脚本语法检查写成真实验收通过。
