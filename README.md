# papertrade-rust

[Papertrade](https://docs.papertrade.xyz/)（HyperEVM 上的合成永续）的双账户对冲机器人，Rust 版。两个专用账户在同一市场一多一空、等额开仓，价格偏离开仓价达到阈值（默认 100 bps）时两边一起平仓，然后重复。亏损的一边按亏损额铸造 PAPER，两边合计的净损耗就是获得 PAPER 的成本。

这是 [TypeScript 版](https://github.com/AnalyThothAI/papertrade) 的重写：策略规则相同，多了本地数据库、操作员控制和终端面板，形成"决策 → 执行 → 落账 → 可见 → 人工干预"的闭环。

> 协议没有公开 API。线路协议来自官方前端（`exchange.papertrade.xyz`）的实际行为，随时可能变化。合约是可升级代理，资金风险自负。
> 两个版本**不要同时对同一组账户运行**：它们互不知情，会各开一轮。

## 结构

```
                    ┌──────────────── papertrade run（常驻进程）────────────────┐
 账户状态流 (SSE) ─▶│                                                          │
 Hyperliquid 盘口 ─▶│  收集已知信息 ─▶ decide()（纯函数）─▶ 签名并提交意图     │─▶ 中继
 交易开关 / 队列  ─▶│        ▲                    │                            │
                    └────────│────────────────────│────────────────────────────┘
                             │ 读                 ▼ 写
                    ┌────────┴───────── data/papertrade.db（SQLite）───────────┐
                    │ control 操作员意图 │ status 实时快照 │ events 时间线      │
                    │ intents 意图预写日志 │ trades 成交台账                    │
                    └────────▲───────────────────▲─────────────────────────────┘
                             │ 写 control        │ 只读
                  pause / resume / flatten     tui · status · 任何 SQLite 工具
```

- **机器人只有一个写入方**：常驻进程。面板和命令行都通过数据库和它交互，随时可开可关，不影响交易。
- **数据库就是机器人相信的事实**：它看到什么、打算做什么、做过什么、结果如何，都能直接查。
- **意图先落盘再发送**：重启后会接着跟踪上次没有落定的意图，不会因为"不知道发没发出去"而重复开仓。

## 快速开始

```bash
cargo build --release
cp .env.example .env                       # 填入两个钱包地址
./target/release/papertrade run            # PT_DRY_RUN=true：只决策和记录，不提交
./target/release/papertrade tui            # 另开一个终端看面板
```

每个账户注册一把机器人自己的 session key（30 天有效，只能开/平仓，不能提现）：

```bash
papertrade session-key new long
# 按提示用主钱包对生成的 EIP-712 请求签名，例如：
#   cast wallet sign --data --from-file secrets/<钱包>.registration.json --interactive
papertrade session-key submit long --signature 0x…
```

session key 文件格式与 TypeScript 版相同，两个仓库可以共用同一个 `secrets/` 目录。

## 命令

| 命令 | 作用 |
|---|---|
| `run` | 运行机器人；交易未开放时只会等待，开放后自动开始 |
| `tui` | 终端面板：当前决策、行情与偏离、两个账户、台账、时间线；`p` 暂停/恢复，`q` 退出 |
| `status` | 打印一次当前状态（机器人没在运行时显示最后一次状态和时间） |
| `pause` | 不再开新一轮；已开的一轮照常持有到目标 |
| `resume` | 恢复开仓 |
| `flatten` | 立刻平掉两个账户的所有仓位，之后不开仓，直到 `resume` |
| `session-key new\|submit <long\|short>` | 创建并注册 session key |

想要网页视图，不需要额外代码：[Datasette](https://datasette.io) 可以直接把数据库变成可浏览、可查询的网站。

```bash
uvx datasette data/papertrade.db      # http://127.0.0.1:8001，表 events / intents / trades_usd
```

## 配置

命令行参数或环境变量（`.env` 会被自动读取）。

| 变量 | 默认 | 含义 |
|---|---|---|
| `PT_LONG_ACCOUNT` / `PT_SHORT_ACCOUNT` | — | 做多 / 做空账户的主钱包地址 |
| `PT_KEYS_DIR` | `./secrets` | session key 目录（`<钱包>.key`，权限 0600） |
| `PT_DATA_DIR` | `./data` | 数据库目录 |
| `PT_COIN` | `BTC` | 市场 |
| `PT_NOTIONAL_USD` | `10000` | 每腿仓位价值；协议下限为 10,000 |
| `PT_LEVERAGE` | `20` | 杠杆；保证金 = 仓位价值 ÷ 杠杆 |
| `PT_CLOSE_BPS` | `100` | 平仓阈值；必须不超过强平距离的一半 |
| `PT_REQUIRE_EMPTY_QUEUE` | `true` | 仅在 LP 派彩队列为空、且两个账户都没有排队款时开仓 |
| `PT_INTENT_TTL_SECONDS` | `120` | 每个签名意图的有效期 |
| `PT_DRY_RUN` | `true` | 只决策和记录，不提交 |

## 它如何决策

整个策略是一个纯函数 [`decide`](src/strategy/decide.rs)。每个"等待"都对应 [`Wait`](src/strategy/decide.rs) 里一个具体、可检查的原因，面板和时间线显示的就是它。

| 账户状态 | 动作 |
|---|---|
| 状态未知，或有意图尚未落定 | 等待 |
| `flatten` 模式 | 平掉所有仓位（包括预期之外的），不开仓 |
| 两边都没有仓位 | 所有开仓条件满足时两腿同时开仓；`pause` 模式下等待 |
| 两边各有一个方向正确的仓位 | 偏离达到阈值时平仓，亏损腿先平 |
| 只有一边有仓位 | 立刻平掉剩下的那条腿 |
| 出现预期之外的仓位 | 不动，等人工处理或 `flatten` |

安全规则：

- **平不了就不开**：中继暂停平仓时不会开仓。
- **结果未知不重发**：提交结果不明的意图在其有效期内可能仍会执行，机器人一直等到账户状态给出结论或链上时间越过有效期。
- **开仓有冷却**：每次开仓尝试后，等一个意图有效期再允许下一次开仓。
- **数据不新鲜就不动**：账户流中断或静默、行情超过 5 秒没更新时一律等待。账户流每 30 秒主动重连对账一次。
- **数据库写不进去就不发**：意图预写失败时不会提交。

## 数据库

| 表 | 内容 |
|---|---|
| `control` | 操作员意图：`run` / `pause` / `flatten`，一行 |
| `status` | 机器人当前看到和打算做的事（JSON 快照），约每秒重写，一行 |
| `events` | 时间线：每次决策变化、每个意图的提交结果、每笔结算、每次模式变更 |
| `intents` | 意图预写日志：请求原文、有效期、状态（`signed → accepted / rejected / unknown → settled`）、中继报告的结果 |
| `trades` | 已结算仓位台账，来自账户成交历史；金额为精确的 18 位小数字符串 |
| `trades_usd`（视图） | 台账的浮点数版本，供显示、SQL 汇总和看板使用，不用于记账 |

## 组件

优先用成熟的开源库，没有自己实现协议栈之外的东西。

| 用途 | 选择 | 原因 |
|---|---|---|
| Hyperliquid 行情 | [`hypersdk`](https://github.com/infinitefield/hypersdk) | 在维护，基于 alloy，自带断线重连和重新订阅；官方 `hyperliquid_rust_sdk` 仍依赖已停止维护的 ethers |
| EIP-712 签名 | [`alloy`](https://github.com/alloy-rs/alloy) | `sol!` 直接从结构体定义生成类型哈希；hypersdk 也用它，不引入第二套密码学 |
| HTTP / SSE | `reqwest` + [`eventsource-stream`](https://crates.io/crates/eventsource-stream) | 账户流需要自己控制重连和定时对账，只用解析器即可；`reqwest-eventsource` 还停在 reqwest 0.12 |
| 数据库 | [`sqlx`](https://github.com/launchbadge/sqlx)（SQLite，WAL） | 异步、自带迁移；WAL 让面板在机器人写入时照常读取 |
| 终端面板 | [`ratatui`](https://ratatui.rs) + `crossterm` | 单个二进制、走 SSH 就能看、不暴露端口 |
| 网页视图 | [Datasette](https://datasette.io)（可选，外部工具） | 现成的 SQLite 网页界面，不值得为两个账户自己写前后端 |
| 命令行与配置 | `clap`（含环境变量）+ `dotenvy` | 参数、环境变量、`.env` 一套定义 |
| 日志与错误 | `tracing`、`anyhow`、`thiserror` | 生态标准 |

## 与 TypeScript 版的对照

| | TypeScript 版 | Rust 版 |
|---|---|---|
| 策略规则 | 相同 | 相同，另加 `pause` / `flatten` |
| 签名 | viem | alloy；测试里用同一把测试密钥验证两边请求体与意图 ID 逐字节一致 |
| 状态 | 无持久化，全部从账户状态推导 | 同样推导决策；另把意图、事件、成交写入 SQLite |
| 重启安全 | 启动后等一个意图有效期再开仓 | 从预写日志恢复在途意图，无需等待 |
| 可见性 | 日志 + `status` 命令 | 终端面板、`status`、时间线、台账、任意 SQLite 工具 |
| 人工干预 | 停进程 | `pause` / `resume` / `flatten` |
| 部署 | Node 24 + `node_modules` | 单个二进制 |

## 已知限制

- **平仓只能走中继**。合约不接受用户直接开/平仓；持仓期间若中继暂停平仓，一条腿可能被强平而另一条腿无法关闭。`flatten` 同样受此限制。
- **盈利可能排队**。LP 付不出时，盈利部分进入 FIFO 派彩队列。
- **两腿不是原子的**，可能落在不同区块，也可能只成功一条（随后被立即平掉）。
- **资金不自动调拨**。某个账户余额不足一份保证金时停止开仓，需要人工调拨。
- 截至 2026-10-10，交易尚未开放（中继暂停 `openPosition` 与 `closePositions`）。session key 注册已对线上中继验证；开仓、平仓、成交台账的字段要等开放后才能端到端确认。台账里"净盈亏 = 冲击后盈亏 − 用户支付的手续费"是按前端字段含义做的假设。

## 开发

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

| 模块 | 内容 |
|---|---|
| `src/protocol/` | 线路协议：部署常量、EIP-712 意图、数据格式、账户状态折叠、中继客户端、账户流 |
| `src/market.rs` | Hyperliquid 盘口行情 |
| `src/strategy/` | 决策函数、在途意图跟踪 |
| `src/store.rs`、`migrations/` | SQLite 存储与表结构 |
| `src/status.rs` | 写入数据库的状态快照 |
| `src/bot.rs` | 运行循环 |
| `src/tui.rs` | 终端面板 |
| `tests/fixtures/` | 生产环境抓取的真实响应，以及 TypeScript 版生成的签名金标准 |
