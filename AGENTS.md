# AGENTS.md

papertrade-rust 是 [Papertrade](https://docs.papertrade.xyz/) 的双账户对冲机器人：两个专用账户在同一市场一多一空，偏离达到阈值时一起平仓。它会用真实资金下单。用法、结构、决策规则和异常场景见 [README](README.md)，这里只写改动这个仓库时要遵守的规则。

## 工作区：主 checkout 永远在 main

主 checkout（克隆下来的那个目录）有两个用途：跟随 `origin/main`，以及运行。实际在跑的 `papertrade run` 用的就是它的 `target/release/papertrade`、`.env`、`data/`、`secrets/`。所以：

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
```

然后在运行它的那个终端里停掉 `papertrade run`（`Ctrl-C`）再启动。启动时要在终端里输入一次解锁密码，这一步只能由使用者自己做。不从 worktree 或未合并的分支运行会下单的进程。

本工具是一个普通的命令行程序加面板。它在哪个终端、哪个复用器里跑，要不要开机自启，都由使用者自己决定：仓库里不放 systemd 单元、tmux 或 zellij 配置，代码也不依赖它们。需要长期在线的能力（断线重连、任务失败后重新开始）做在程序自己里面。

需要用户明确同意的事：把 `PT_DRY_RUN` 改成 `false`、把 `PT_WALLET_AUTOMATION` 或 `PT_REBALANCE` 改成 `true`、调整仓位或杠杆、注册或更换 session key、动用任何资金、删除 `data/` 或 `secrets/`。真实的私钥和解锁密码只由使用者在终端里输入：不要读取、索要、代为输入它们，也不要对真实的 `secrets/` 运行 `keys init`。`PT_WALLET_AUTOMATION=true` 且主钱包私钥已解锁时，机器人自己注册 session key；再打开 `PT_REBALANCE=true`（默认关闭），它才会自己在两户之间转账。这些是使用者打开开关并导入私钥时已经同意的行为；代理不用这些私钥做别的事。

## 改代码时

- **决策留在纯函数里。** [`decide`](src/strategy/decide.rs)、[`track`](src/strategy/inflight.rs)、[`funds`、`hold`、`reserved`](src/strategy/rebalance.rs) 和 [`next_step`](src/strategy/transfer.rs) 不做 I/O、不读时钟；循环（[`bot.rs`](src/bot.rs)、[`bot/funding.rs`](src/bot/funding.rs)）只负责收集输入、执行、记录。新规则加在这些函数里，而不是散在循环中。
- **资金安全规则不能悄悄改。** 结果未知的开仓在链上时间越过有效期之前不重发；开仓凭证据落定，平仓可以重发；平不了就不开；平仓不依赖数据库，也不被资金判断或转账挡住；数据不新鲜就等待。再平衡和交易互不等待：`decide` 不知道转账的存在，开仓不因为转账在计划、进行或卡住而停下；唯一的联系是转账已占用、还可能从转出方余额里提走的金额，开仓的余额检查不把它算作可用。再平衡默认关闭，关闭时不发起新的转账，已经开始的转账仍然走完。转账的每一步先落盘再执行，只凭余额确认完成；结果未知的提现在有效期过去之前不重签；一笔转账在 Hyperliquid 上只用一个 nonce；同一时间最多一笔转账；转出方有开仓或平仓在途时不发起转账。改动其中任何一条，要在 PR 里说明为什么，并更新对应场景。
- **行为变化要有场景测试。** 涉及两条腿、中继、账户流、重启的改动，在 [`tests/rounds.rs`](tests/rounds.rs) 里加或改一个场景；模拟交易所（[`tests/support/`](tests/support/mod.rs)）不够用就扩它。确认新测试在改动被撤回时会失败。
- **协议是逆向来的。** 线路格式、地址、EIP-712 类型来自官方前端。改它们要说明证据（抓到的响应、前端代码、线上实测），并保持 `tests/fixtures/` 里的真实样本和签名金标准通过。
- **金额不用浮点。** 线路和记账用整数（`U256` / 十进制字符串）；`f64` 只出现在显示和 `trades_usd` 视图里。
- **不造第二套。** 一个行情来源、一个账户流、一个意图日志、一个决策函数。优先用成熟的库，新增依赖要有理由。
- **密钥只有一个出入口。** 密钥文件只由 [`keystore`](src/keystore.rs) 读写：一种格式（V3 密文）、从不覆盖已有文件、读之前先检查格式、主钱包私钥要核对地址。`run` 在启动时解锁一次，把 `UnlockedKeys` 交给循环；`bot.rs`、策略、行情、存储都不读密钥文件、不提示密码，每一拍里没有解密。不加明文回退，不加第二种存储或"密钥提供方"抽象。
- **主钱包私钥只签三种东西。** 解锁出来的主钱包私钥交给 [`src/wallet.rs`](src/wallet.rs) 的 `WalletSigner`，它不向外交出签名器，只签：注册该钱包的 session key、把该钱包的余额提现到它自己的 HyperCore 账户、在 Hyperliquid 上把 USDC 转到**对方账户的存入地址**。转账的收款地址不是参数，而是用对方钱包地址从链上的存入地址工厂读出来的。不要增加第四种签名、转到任意地址的入口、或从中继的回复里取收款地址；确有必要时在 PR 里单独说明，并更新 `wallet.rs` 里对应的测试。
- **秘密不进仓库、日志和 PR。** 磁盘上只有 `secrets/` 里的密文（session key；开启 `PT_WALLET_AUTOMATION` 时还有两个专用主钱包的私钥）。密码和明文私钥不进 `.env`、参数、环境变量、数据库、状态快照、日志和报错信息；测试只用随机生成的空钱包。

## 验证

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

三项都过才提交。场景测试有时序，改了循环或模拟交易所之后多跑几遍（`cargo test --test rounds`）确认不是偶发通过。线上验证只做只读请求；任何会被中继接受的写请求都需要用户同意。没跑的验证如实写成没跑。

## 提交与 PR

一个任务一个 PR。PR 说明写清楚：改了什么行为、怎么验证的、哪些没法验证。合并由用户决定，除非用户已经说了直接合并。合并后按上面的步骤回收 worktree；需要上线时再走"上线"一节。
