# 资金协议证据与验收边界

2026-10-10 核对。研究只做读取：官方已加载的前端脚本、公开状态接口和 GET 请求。没有签名、提交意图、请求存入地址归集，也没有读取真实私钥、密码或生产 `.env`、`data/`、`secrets/`。以下匿名样本用于核对字段含义，不能作为本地两个钱包的实盘验收。

## 官方前端

| 来源 | 相关行为 |
| :--- | :--- |
| [account-wire-oK7wgoTe.js](https://exchange.papertrade.xyz/assets/account-wire-oK7wgoTe.js) | `lifetime` 含累计入金 `depositedRaw`、累计提现 `withdrawnRaw`、已实现盈亏 `realizedPnlRaw` 与已付费用 `feesPaidRaw`。资金字段是精确十进制字符串。补丁中的 `lifetime` 位于顶层，缺省时沿用前值 |
| [index-Tu4xzYeT.js](https://exchange.papertrade.xyz/assets/index-Tu4xzYeT.js) | 约字符偏移 612523：累计入金/提现增加才触发到账通知；约 416773：存入动作以发送前累计入金为基线。约 470891–471669：历史响应为 `items` 和必需、可为 null 的 `nextCursor`，每页请求 75 条，拒绝重复游标 |
| [execution-CNs4GYCc.js](https://exchange.papertrade.xyz/assets/execution-CNs4GYCc.js) | 约 168131、169853：发送存入前读取累计入金，读不到就不发送；发送结果保存 `depositedBefore` |
| [hooks-C8jAqgfJ.js](https://exchange.papertrade.xyz/assets/hooks-C8jAqgfJ.js) | 净入金按累计入金减累计提现计算，与交易盈亏分开 |
| [PortfolioActivity-BTrz8vJ_.js](https://exchange.papertrade.xyz/assets/PortfolioActivity-BTrz8vJ_.js) | 约 38320：提现 `amountRaw` 是 Exchange 扣款，`recipientAmountRaw` 是扣费后发给收款方的金额 |

这些完整 URL 对应核对时的构建。字符偏移只用于定位这版压缩代码；后续构建可能更换文件名。

## GET-only 匿名核对

- `GET /state/user/live?wallet={wallet}` 返回 200；匿名空账户初始快照有全部 `lifetime` 字段。读取初始事件后关闭 SSE，没有触发资金动作。
- `GET /query/accounts/{wallet}/cashflows?limit=75` 返回 200，响应有 `items`、`nextCursor`。一个已公开显示的匿名账户有两笔 Deposit，金额分别为 `1030259377000000000000`、`30000000000000000000`；一笔已结算 Withdrawal 为 `30000000000000000000`，此样本的 `recipientAmountRaw` 与扣款相等。
- 同账户随后读取的累计 `depositedRaw = 1060259377000000000000`，精确等于两笔入金之和；`withdrawnRaw = 30000000000000000000`，等于已结算提现。已实现盈亏和已付费用为零。这支持实际资金流口径，没有证明带费用提现的行为。
- `GET /query/accounts/{wallet}/trades?filter=all&limit=75` 对匿名空账户返回 200，真实响应保存于 [trades-empty.json](../tests/fixtures/trades-empty.json)：`items` 为空，`nextCursor` 显式为 null。此次接口核对中，`limit` 在 `filter` 前返回 400；实现保持已验证的 `filter`、`limit`、`cursor` 顺序并编码 opaque cursor。

既有 [wallet-snapshot.json](../tests/fixtures/wallet-snapshot.json) 也包含同一 `lifetime` 格式；其中地址已匿名化，金额不是本次空账户余额。读写模型测试验证全量快照、顶层补丁及缺省继承；没有引入第二个资金行情或流水采集者。

## 实现采用的证明

资金字段与余额、仓位在同一账户流中按版本折叠。`accountRevision` 用于同一连接内连续性检查；重连可从 1 开始，因此不把它当成跨重启的持久序号。提现的负面证据还依赖账户的 `indexedThroughTimestamp`，连接心跳不能证明索引已越过旧意图。

| 阶段 | 发请求前保存 | 确认证据 |
| :--- | :--- | :--- |
| Withdraw | 发款方 Core 余额、累计提现、尝试时间和提现意图 | Core 增加与同一账户流的累计提现增加都达到本笔金额，才前进到 Send 并释放提现占用资金；账户流迟到继续等待。只有旧意图已结束、未报告执行成功，且结束 30 秒后的 Core 读数未增加、累计提现不变、账户索引时间足够新，才可重签 |
| Send | Core 余额、收款方累计入金、唯一 nonce | Core 减少达到本笔金额；结果未知的重发沿用同一 nonce |
| Credit | 沿用 Send 前的累计入金基线 | 累计入金增量达到本笔金额，沿用原有 1 USDC 容差；交易盈亏不能充当入金 |

基线先写入现有 `transfers` 记录再发请求，重启不重新取已尝试动作的基线。缺少资金字段时等待；提现计数未保持原基线就不能证明“什么都没发生”。每笔转账仍由同一个串行资金任务处理；资金任务的网络和存储等待发生在后台，交易循环继续推进。

新记录同时保存 `from_wallet`、`to_wallet`，让基线明确属于实际钱包，而不是可配置的 long/short 标签。当前配置不匹配时保持转账阶段，等待恢复原配置，不使用新钱包读数，不发资金请求。Core 先到、SSE 后到时，仍保留 Withdraw 的占用资金，避免用尚未扣除提现的账户余额开仓；平仓不受这次等待影响。

两户应是专用账户。自动转账确认期间不要并行手工入金、提现或移动 Core 资金：累计计数区分资金流和盈亏，但不归属并行资金操作的来源。

## 旧记录与台账

[0006_transfer_wallets.sql](../migrations/0006_transfer_wallets.sql) 为旧行保留未知的钱包归属，不从今日配置补填。原因是旧 Core/资金基线无法单独证明属于当前两钱包。归属检查先于旧基线口径恢复：

- 已有钱包归属但配置不匹配：保持阶段，等待恢复原钱包配置。
- 无钱包归属的旧记录，尚未尝试 Withdraw 或已处于其他阶段：`stuck`，保留记录和原 nonce，由人工核对原钱包和资金去向。
- 无钱包归属且已经尝试 Withdraw：保留阶段和原意图，两侧都保守预留提现金额。日志缺失或意图在途时持续等待；只有日志明确 Ended、结束后已过 10 分钟，且两侧 `indexedThroughTimestamp` 都越过结束后的 10 分钟，才可 `stuck` 并由操作员核对后 `abandon`。绝不从当前 Core 余额推断旧到账。
- 旧 Stuck 保持操作员控制，不自动放弃或重新绑定。

[0004_cashflow_baselines.sql](../migrations/0004_cashflow_baselines.sql) 仍保留旧权益基线并标记 `cashflow_baselines=false`，权益不会被直接当成累计资金。该旧口径逻辑只适用于钱包归属已经确定、配置也一致的记录；通常旧迁移记录归属为空，先进入上述人工恢复路径，不承诺自动凭 Core 恢复。

交易台账继续只读同一 trades 端点，沿 opaque `nextCursor` 到明确末页；后页错误或游标异常时不提交部分历史。[0005_ledger_sync.sql](../migrations/0005_ledger_sync.sql) 持久保存每钱包同步证据。两钱包都成功同步、新鲜且完整才显示 `actual cost`；失败或超过三个轮询间隔显示 `stale`，缺完整性显示 `incomplete`，首次未同步显示 `unknown`。台账同步与交易决定独立。

## 尚待实盘验收

本次证据没有证明机器人真实提现、Hyperliquid 转账、归集入账或真实成交。协议费用、首次账户激活费和真实到账时延还需在用户授权的真实资金验收中逐项核对；当前不足额到账保持保守处理，不新增猜测的费率容差。

PAPER 预估仍有 [README 的已知边界](../README.md#estimate)：LP 现金不足时亏损费豁免、单笔跨 cliff 分段、tail 的合约取整顺序与 trackedLp 回落语义，均未由真实结算逐位验证。已导入私钥、模拟场景通过和公开 GET 响应不能替代这些验收。
