# WorkBuddy 支持（本地分支说明）

本仓库是 [cc-switch](https://github.com/farion1231/cc-switch) v4.0.22 的本地 fork，新增了第 15 个受管应用 **WorkBuddy**（腾讯 WorkBuddy 桌面版）。

## 产物

- 应用包：`src-tauri/target/release/bundle/macos/CC Switch.app`
- 运行：`open "src-tauri/target/release/bundle/macos/CC Switch.app"`（**先退出正在运行的旧版 CC Switch**，两者会抢同一个 `~/.cc-switch/cc-switch.db`）
- 构建结束时的 `TAURI_SIGNING_PRIVATE_KEY` 报错只影响自动更新签名，不影响本地运行

## 数据落点

WorkBuddy 把自定义模型存在 `~/.workbuddy/models.json`，是一个 JSON 数组：

```json
[
  {
    "id": "deepseek-v4.1-flash",
    "name": "deepseek-v4.1-flash",
    "vendor": "Custom",
    "url": "https://api.deepseek.com/v1/chat/completions",
    "apiKey": "sk-...",
    "supportsToolCall": true,
    "supportsImages": false,
    "supportsReasoning": false,
    "useCustomProtocol": false
  }
]
```

本分支把这个"数组里的一条记录"当作一个 provider：**provider 的主键 = 数组元素的 `id`**，`settings_config` 就是这条记录本身，未知字段原样保留。

## 已实现

- 侧栏出现 WorkBuddy 入口（默认可见，可在「设置 → 应用」里关掉）
- 新增 / 编辑 / 删除供应商，直接读写 `~/.workbuddy/models.json`（原子写 + 写前快照回滚 + 进程内互斥锁）
- 共存式（additive）模式：多条供应商可同时存在于 `models.json`
- 独立表单：模型 ID（主键）、显示名称、接口地址、API Key、工具调用 / 图片 / 深度思考 / 自定义协议路径四个开关
- 校验：模型 ID 字符集、ID 重复、URL 必须是 http(s)、API Key 非空
- 「打开配置目录」、配置状态检测、托盘应用名、用量脚本的 base_url/apiKey 提取
- 图标：内置一个自绘的 WorkBuddy 图标（`src/icons/extracted/index.ts`），可随时换成官方 logo

## 用量监控（token 统计）

WorkBuddy 每次模型调用的用量记录在本地会话转录里，本分支新增 `services/session_usage_workbuddy.rs` 把它导入用量看板，**不需要走本地路由**。

**数据源**

| 路径 | 内容 |
|---|---|
| `~/.workbuddy/projects/<workspace>/<sessionId>.jsonl` | 主会话，逐行带 `providerData.usage`（`inputTokens` / `outputTokens` / `inputTokensDetails[].cached_tokens`）、`providerData.model`、`timestamp`(ms) |
| `<sessionId>/subagents/*.jsonl` | 子代理会话，格式同构，一并导入 |

**落库方式**

- 每个带 usage 的行写成一条 `proxy_request_logs` 记录：`app_type='workbuddy'`、`data_source='workbuddy_session'`、`provider_id='_workbuddy_session'`
- `request_id = workbuddy:<sessionId>:<消息id>`（无 id 时用整行指纹），`INSERT OR IGNORE` 保证幂等
- 文件按 `session_log_sync.last_byte_offset` 增量读；文件变短（被重写）时从头重扫
- 不完整的尾行（没有换行符）留到下次同步

**口径**：WorkBuddy 的 `inputTokens` 按 OpenAI 约定**包含**缓存命中，所以行以 `INPUT_TOKEN_SEMANTICS_TOTAL` 入库，并把 `workbuddy` 加进 `sql_helpers::CACHE_INCLUSIVE_APP_TYPES`（前端 `src/types/usage.ts` 的白名单同步），聚合时再减掉缓存。`reasoning_tokens` 已包含在 `outputTokens` 里，不重复累加。

**耗时与速度**：转录里没有耗时段，`latency_ms` 按"上一行工具返回（`function_call_result`）→ 本条 usage 记录"的时间差估算。不能取"上一行"，因为每条 usage 前面紧挨着一条 `reasoning`（早十几毫秒），会把耗时算成 15ms、速度恒为 0。间隔超过 120 秒视为用户走开/会话挂起，直接丢弃（截断到上限会把整体速度从 84 拉低到 40 tok/s）。因此速度走界面的**估算通道**：带 `≈` 前缀，门槛 `输出≥200 token` 且 `耗时≥1s`。**首字时间无法估**（数据源没有 TTFT），那一列对 WorkBuddy 恒为 `—`。

**成本**：`find_model_pricing` 查价格表；WorkBuddy 用的模型大多不在内置 models.dev 列表里，需要到「用量 → 价格配置」里给这些模型补单价，否则成本记 0（token 照常统计）。

## 未实现（有意留白）


| 能力 | 现状 |
|---|---|
| 本地路由 / 聚合 | 不支持。WorkBuddy 走直连，`supports_local_proxy()` 显式排除 |
| 积分统计 | `workbuddy.db` 的 `session_usage.credit_json`（每条消息积分）暂未导入，当前只有 token |
| MCP 同步 | **已支持**。写入 `~/.workbuddy/connectors/default/mcp.json` 的 `mcpServers`，键名加 `ccswitch:` 前缀，其他 connector 条目原样保留；stdio（command/args/env）与远程（url/type/headers）都支持 |
| Skills 同步 | **已支持**。复制/链接到 `~/.workbuddy/skills/<name>/`（WorkBuddy 原生 skill 目录） |
| 会话管理 | 不做 |
| 目录自定义 | 不支持覆盖 `~/.workbuddy`，可临时用 `WORKBUDDY_HOME` 环境变量（测试用） |
| 深链接导入 | 显式拒绝，提示从 WorkBuddy 页添加 |
| 协议判断 | 不做。只校验 URL 形状；若上游只支持 Responses API，WorkBuddy 会调用失败，需自行确认 `/chat/completions` 可用 |

## MCP 与 Skills

- **MCP**：新增 `src-tauri/src/mcp/workbuddy.rs`。WorkBuddy 的用户级 MCP 在 `connectors/default/mcp.json`（131 条 connector 条目），写入时只动 `ccswitch:` 前缀的键，`disabled: false`；远程条目未指定 type 时默认 `streamableHttp`。数据库 `mcp_servers` 增加 `enabled_workbuddy` 列（schema v25）。
- **Skills**：`skill_apps` 增加 `enabled_workbuddy`，同步目标目录是 `~/.workbuddy/skills`（与 WorkBuddy 自己的 `SKILL.md` 目录结构一致）。注意 cc-switch 本地 skill 库为空时，Skills 页只会显示「还没有 Skill」，导入/添加一个才会出现应用矩阵里的 WorkBuddy 列。

## 改动清单

**新增**

- `src-tauri/src/workbuddy_config.rs`（311 行）：models.json 的读写、校验、原子写与回滚、3 个单测
- `src/components/providers/forms/WorkbuddyProviderForm.tsx`（329 行）：供应商表单
- `src-tauri/src/services/session_usage_workbuddy.rs`：会话转录 → 用量库导入器（含 3 个单测：解析/去重/增量）

**修改**：33 个文件 / +173 −24 行，主要是 `AppType` 新增变体后各处的穷尽 match，以及前端 `AppId` 联合类型、`Record<AppId, …>` 表、列表与弹窗的白名单。

关键接线点：

- `src-tauri/src/app_config.rs`：`AppType::Workbuddy`、`is_additive_mode()`、`FromStr`
- `src-tauri/src/services/provider/live.rs`：`write_live_snapshot`（唯一落盘点）、`provider_exists_in_live_config`（漏了会导致"编辑不落盘"）
- `src-tauri/src/services/provider/mod.rs`：`validate_provider_settings`、`delete` / `remove_from_live_config` 分支
- `src/config/appConfig.tsx`：`APP_IDS`、`DEFAULT_VISIBLE_APPS`、`APP_ICON_MAP`

## 重新构建

```bash
# 前端（国内网络）
npm install --registry=https://registry.npmmirror.com --legacy-peer-deps
npm run build:renderer

# 应用包（--config 覆盖掉需要 pnpm 的 beforeBuildCommand）
npx tauri build --bundles app --config '{"build":{"beforeBuildCommand":""}}'
```

注意：`@tauri-apps/api` 等 4 个 npm 包必须锁在 `2.10.1 / 2.3.1 / 2.6.0 / 2.10.0`，与 Rust crate 的 major.minor 对齐，否则 Tauri 会以版本不一致拒绝构建。

## 与官方上游的关系

官方仓库没有"自定义应用"扩展点（12 个应用是固定枚举，无插件 SDK），所以只能 fork。上游升级时冲突点集中在 `app_config.rs` 的枚举与各 match，以及 `live.rs` 的落盘点，本文件可当合并 checklist。
