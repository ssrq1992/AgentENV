# Native SSF overlay

This overlay extends the original base. It preserves Gateway, Scheduler, node DaemonSet, headless discovery, heartbeats, P2P configuration, probes, and native termination policy.

Render it through `SSF/scripts/render_agentenv_native.py`; the original base expects `config/agentenv.toml` to be materialized from `AgentENV/config/default.toml`. The helper does this in a temporary directory and never applies resources. Replace the CSI driver/controller settings and provide the generated role Secrets. Common SSF Guard/RBAC and the actual CSI driver remain external prerequisites.

The cluster must support native sidecars (`initContainers[].restartPolicy: Always`). SSF Node starts first and exits last. NodePlugin keeps evidence/release service available during its bounded drain. Persistent WAL and reference state survive incomplete drain.

See [deployment instructions](../../../../../design/13-agentenvadapter-installation-and-deployment.md) and [implementation status](../../../../../design/18-native-integration-implementation-status.md). This is an incremental deployment artifact, not completed lifecycle certification. The current public lifecycle gates remain in place.

能力协商：原生节点必须报告 `agentenv.storage.v2` / hook version `3`。控制配置要求完整 `agentenv-capture-clone-v1`；NodePlugin 广告已实现的 Block/RO/Freeze/Capture/Restore/RetainedUse/OfflineExpand。协议身份不代表真实厂商 CSI 验收。未知协议、能力或节点身份不准入，停止清理仍使用独立身份校验。
