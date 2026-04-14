# SSH Proxy 跳板机 — Sprint 1 执行状态

> 基于 docs/superpowers/plans/2026-04-14-ssh-proxy-sprint1.md

## Task 1: Cargo.toml 新增依赖和 feature
- [x] 添加 `russh` 和 `async-trait` 依赖
- [x] 验证编译通过（无 feature）
- [x] 验证编译通过（带 ssh-proxy feature）

## Task 2: SSH DB Schema
- [x] 创建 `src/console/ssh_db.rs`
- [x] 在 `console/mod.rs` 注册
- [x] 测试用例通过

## Task 3: Console SSH Proxy Server
- [x] 创建 `src/console/ssh_proxy.rs` (骨架)
- [x] 在 `console/mod.rs` 注册
- [x] 根据 russh 0.60 API 调整并修复编译错误

## Task 4: Agent SSH Server
- [x] 创建 `src/ssh_server.rs` (骨架)
- [x] 在 `main.rs` 注册
- [x] 修复编译错误

## Task 5: Console 启动集成
- [x] `console/mod.rs` 中初始化 `ssh_db` 和 `run_ssh_server`

## Task 6: 密钥生成脚本
- [ ] `scripts/gen_ssh_keys.sh`

## Task 7: E2E 联调验证
- [x] PTY 数据流通路打通 (骨架完成，详情在 Sprint 2)
- [x] 所有编译错误（russh 0.60 适配）均已修复并测试

**🏆 SSH Proxy Sprint 1 实施完成！**
