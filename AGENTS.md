# AGENTS.md

papertrade-rust 是 [Papertrade](https://docs.papertrade.xyz/) 的双账户对冲机器人：两个专用账户在同一市场一多一空，偏离达到阈值时一起平仓。它会用真实资金下单。用法、结构、决策规则和异常场景见 [README](README.md)，这里只写改动这个仓库时要遵守的规则。

## 工作区：主 checkout 永远在 main

主 checkout（克隆下来的那个目录）有两个用途：跟随 `origin/main`，以及运行。常驻服务用的就是它的 `target/release/papertrade`、`.env`、`data/`、`secrets/`。所以：

- **不在主 checkout 切分支、提交、改文件。** 它只做 `git pull --ff-only`。
- **每个任务一个 worktree**，从最新的 `origin/main` 开出，分支名即任务名：

  ```bash
  git fetch origin main
  git worktree add .worktrees/<任务名> -b <任务名> origin/main
  cd .worktrees/<任务名>
  ```

- 编辑、构建、测试、提交、推送、开 PR 都在自己的 worktree 里完成。
- 多个任务可以同时进行，各用各的 worktree，互不影响。不要 `reset`、`stash`、`clean`、切换或删除别人的 worktree 和分支。
- **PR 合并或关闭后回收**：

  ```bash
  git worktree remove .worktrees/<任务名>
  git branch -d <任务名>
  ```

- `.worktrees/` 已被忽略。每个 worktree 有自己的 `target/`，不要把 `CARGO_TARGET_DIR` 指向主 checkout 的 `target/`：在那里构建 release 会替换正在运行的二进制。
- worktree 里没有 `.env`、`data/`、`secrets/`，也不需要：测试用临时目录和模拟交易所。不要把主 checkout 的这些文件复制或链接进来，更不要提交。

## 上线：只从 main

合并之后，在主 checkout 里：

```bash
git pull --ff-only
cargo build --release
systemctl --user restart papertrade
papertrade status
```

不从 worktree 或未合并的分支运行会下单的进程。常驻运行用 systemd 用户服务（[`deploy/papertrade.service`](deploy/papertrade.service)），不靠某个终端会话活着；看面板用 zellij 布局（[`deploy/dashboard.kdl`](deploy/dashboard.kdl)）。同一个数据目录只允许一个 `run` 进程。

需要用户明确同意的事：把 `PT_DRY_RUN` 改成 `false`、调整仓位或杠杆、注册或更换 session key、动用任何资金、删除 `data/` 或 `secrets/`。

## 改代码时

- **决策留在纯函数里。** [`decide`](src/strategy/decide.rs) 和 [`track`](src/strategy/inflight.rs) 不做 I/O、不读时钟；循环（[`bot.rs`](src/bot.rs)）只负责收集输入、执行、记录。新规则加在这两个函数里，而不是散在循环中。
- **资金安全规则不能悄悄改。** 结果未知的开仓在链上时间越过有效期之前不重发；开仓凭证据落定，平仓可以重发；平不了就不开；平仓不依赖数据库；数据不新鲜就等待。改动其中任何一条，要在 PR 里说明为什么，并更新对应场景。
- **行为变化要有场景测试。** 涉及两条腿、中继、账户流、重启的改动，在 [`tests/rounds.rs`](tests/rounds.rs) 里加或改一个场景；模拟交易所（[`tests/support/`](tests/support/mod.rs)）不够用就扩它。确认新测试在改动被撤回时会失败。
- **协议是逆向来的。** 线路格式、地址、EIP-712 类型来自官方前端。改它们要说明证据（抓到的响应、前端代码、线上实测），并保持 `tests/fixtures/` 里的真实样本和签名金标准通过。
- **金额不用浮点。** 线路和记账用整数（`U256` / 十进制字符串）；`f64` 只出现在显示和 `trades_usd` 视图里。
- **不造第二套。** 一个行情来源、一个账户流、一个意图日志、一个决策函数。优先用成熟的库，新增依赖要有理由。
- **秘密不进仓库、日志和 PR。** session key 只在 `secrets/`；主钱包私钥不应出现在这台机器上。

## 验证

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

三项都过才提交。场景测试有时序，改了循环或模拟交易所之后多跑几遍（`cargo test --test rounds`）确认不是偶发通过。线上验证只做只读请求；任何会被中继接受的写请求都需要用户同意。没跑的验证如实写成没跑。

## 提交与 PR

一个任务一个 PR。PR 说明写清楚：改了什么行为、怎么验证的、哪些没法验证。合并由用户决定，除非用户已经说了直接合并。合并后按上面的步骤回收 worktree；需要上线时再走"上线"一节。
