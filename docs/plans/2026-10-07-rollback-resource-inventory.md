# Windows 回退资源库存与安装证据

此表基于 3.x 功能分支的实际写入点。用于实现快照白名单，不代表这些资源的恢复已经完成。用户数据不在本机参与演练；编译、真实安装和恢复验证使用 GA 隔离资源。

## 安装证据

- CLI 固定为 Tauri 2.11.5，模板保存在 `src-tauri/nsis/installer.nsi`。
- 原模板的维护页可能先卸载，再进入 PREINSTALL。当前模板对 NSIS 升级跳过维护页，安装 Section 在 PREINSTALL 前要求应用正常退出。
- 原模板构建探针：`https://github.com/qwqwd65-ui/cc-switch/actions/runs/37485907318`，成功。
- 修改后模板构建探针：`https://github.com/qwqwd65-ui/cc-switch/actions/runs/37490056943`，成功。这里只验证生成脚本和构建，不能据此宣称实际回退通过。
- 实际安装探针：`scripts/probe-windows-installers.ps1`。用固定旧生产 Setup 和高于旧版的测试 Setup，验证 `/S`、`/P`、无 `/R`、无 `/UPDATE` 的手动升级、中文空格路径、HKCU 范围、完整版本、安装文件哈希、降级后重装当前程序，以及恢复期间 deny-execute ACL 是否保留。运行 `37572915283` 已通过这些安装合约；没有启动业务 GUI，也没有据此验证产品的完整数据恢复。
- 旧生产资产固定为 `v3.20.4-fork.3`，Setup SHA-256：`27329df67ca6d6b783c76444dad0d99f2c27701ccdac37afebab619a98295a8b`，来源为 GitHub Release asset digest。该探针固定输入哈希；产品 A/B 路径仍需独立 minisign 验证，不把这个哈希当成签名。

## 写入点、解析器和捕获策略

| 对象 / 实际写入点 | 来源解析器 | 快照与恢复策略 |
| --- | --- | --- |
| CC Switch 业务 DB；`database/dao/*`、用量扫描、代理日志、同步导入 | `config::get_app_config_dir()` + `cc-switch.db`；app store override 优先，Windows 兼容遗留 HOME | SQLite Backup API 含已提交 WAL；记录 user_version；恢复时使用独立执行器，不调用 `Database::init()` 或普通 restore；新库 WAL/SHM 必须在应用停止后处理 |
| 设备设置；`settings::save_settings()`、OAuth 迁移标记和同步状态写入 | `AppSettings::settings_path()`；真实 home 下 `.cc-switch/settings.json`，独立于业务目录 | 原始字节、缺失状态和 DACL；恢复后显式禁用云自动同步，提示用户重新确认同步方向 |
| 业务目录 override；`app_store::set_app_config_dir_to_store()` | Tauri AppData/`com.ccswitch.desktop/app_paths.json`，Windows 使用 Roaming AppData | helper 只读 JSON 解析；记录原始 Store 和原业务目录；不能启动 Store 创建/改写文件 |
| Claude Live/供应商配置；`services/provider/live.rs`、`config` | `get_claude_settings_path()`、`get_provider_config_path(id,name)`；覆盖目录；现存 `settings.json`/遗留 `claude.json` 选择 | 列举当前 DB 引用的供应商配置路径；两个候选文件都记录存在状态，防止升级后新增文件改变旧版路径选择；不扫描复制整个 `.claude` |
| Claude MCP；`mcp/claude.rs` | `get_claude_mcp_path()`；默认 home 的 `.claude.json`，自定义目录还有位置选择逻辑 | 精确文件原字节和缺失状态；不假设它一定在 `.claude/` 内 |
| Codex Live；`write_codex_live_atomic()`、MCP 和代理接管/恢复 | `get_codex_auth_path()`、`get_codex_config_path()`、`get_codex_model_catalog_path()` | auth/config/受管模型目录同一快照；权限和缺失状态一并恢复；客户端自己的 `models_cache.json` 只读，排除 |
| Codex 供应商文件和 managed OAuth 标记 | `get_codex_provider_paths(id,name)`；私有 `get_codex_managed_oauth_live_auth_marker_path()` 在业务目录 | 必须暴露无副作用的库存接口；按数据库供应商引用列举 auth/config；标记跟 auth 和账号 DB 同步恢复 |
| Gemini provider/MCP；`gemini_config`、`mcp/gemini.rs` | `get_gemini_env_path()`、`get_gemini_settings_path()`，尊重 override | `.env` 与 settings 精确捕获；不复制会话目录 |
| Grok provider/MCP；`grok_config`、`mcp/grokbuild.rs` | `get_grok_config_path()` | TOML 原字节和不存在状态 |
| OpenCode provider/MCP/plugin 列表；`opencode_config` | `get_opencode_config_path()`、`get_opencode_env_path()` | JSON/JSONC 候选与 env；候选选择条件必须跟原解析器一致；`get_opencode_db_path()` 对应客户端会话库，排除 |
| OMO / OMO Slim；`OmoService::write_profile_config/delete_config_file` | STANDARD/SLIM `config_candidates`；统一 home `.omo/omo.jsonc` 或 `omo.json` 优先，legacy OpenCode 目录其次 | 保存实际选中路径及候选文件存在状态；统一文件有其他客户端段落，恢复丢弃范围需明确覆盖原文件字节；不把它遗漏在 OpenCode 目录之外；UNC/WSL 第一版拒绝 |
| OpenClaw provider/MCP；`openclaw_config` | `get_openclaw_config_path()` | 原 JSON5 字节；自动创建的普通备份不作为版本历史链 |
| Hermes provider/MCP/模型开关；`hermes_config` | `get_hermes_config_path()`；settings override > HERMES_HOME > 平台默认，Windows LocalAppData/hermes | YAML 原字节；原环境路径由清单固定，恢复时不能重新猜默认 `.hermes` |
| Hermes memory 编辑；`write_memory()`；全局提示词 | `get_hermes_dir()/memories/MemoryKind::filename()`；`prompt_file_path(Hermes)` 的 SOUL.md | 保存可编辑的具体 memory 文件和 SOUL.md；客户端会话 DB 和其他自动生成记忆不整体回滚 |
| Pi provider 和默认模型；`pi_config` / `services/provider/pi.rs` | `get_pi_agent_dir()`：settings override > PI_CODING_AGENT_DIR > 默认；`get_pi_models_path()`、`get_pi_settings_path()` | 两文件捕获，客户端只读 auth.json 不纳入批量删除；路径为实际来源而非环境变更后的新目录 |
| Pi 提示词；`PiInstructionService`、`PiPromptFileService`、`PiPromptTemplateService` | Pi agent dir 中固定提示词文件、AGENTS.md 和 `prompts/<slug>.md` | 固定文件精确捕获；模板按受管写入账本列举，不整体复制或清空 prompts；重命名需记录旧、新两路径 |
| MiniMax MCode provider/MCP/prompt；`mcode_config`、`mcp/mcode.rs` | `mcode_config::data_dir()` 中 config.yaml、mcp.json、AGENTS.md；默认 `.minimax` | 原字节和缺失状态；使用实际 resolver；不复制 session/project |
| Claude Desktop 模式/企业 profile；`apply_profile_to_paths()`、`restore_profile_to_paths()` | `current_platform_paths()`；Windows LocalAppData 下发现的 Claude/Claude-3p 目录 | `normal_config_path`、`threep_config_path`、CC Switch 专属 `profile_path` 与 `_meta.json` 四文件一起捕获；暴露无副作用库存接口，不复制整个 ConfigLibrary 或客户端目录 |
| 各客户端全局提示词；`PromptService` | `prompt_files::prompt_file_path(AppType)` | Claude CLAUDE.md、Codex/Grok/OpenCode/OpenClaw/Pi/MCode AGENTS.md、Gemini GEMINI.md、Hermes SOUL.md；数据库启用标记与原字节一致 |
| Skill source、部署副本与软链接；install/update/uninstall/import/migrate/toggle | `SkillService::get_ssot_dir()` 与 `get_app_skills_dir(app)`；SSOT 位于业务目录或 `.agents/skills`；所有客户端 override | 从 DB `skills` 表和 `get_all_installed_skills()` 枚举受管目录；现有 SSOT resolver 会 `create_dir_all`，helper 不能直接调用；链接记录目标和 DACL，不递归跟随；copy 部署保存受管内容；新增/重命名/删除需持久化所有权账本；同名外部目录排除 |
| 安装状态；NSIS 注册表和快捷方式 | HKCU 卸载键/制造商键、InstallLocation、MainBinaryName、原安装目录 | 由固定已验证 Setup 管理，只核对本产品项；不整体导出注册表；恢复时屏蔽旧 exe 启动，验证原目录/范围/版本和 exe 哈希 |
| 便携标记 | exe 同目录 portable.ini | Setup 成功路径记录、移除；恢复遵从原清单；不在普通启动删除，不改变真正 ZIP 的识别 |
| 会话历史、项目、普通备份/skill-backups、模型下载缓存、日志 | 既有客户端/用户路径 | 排除版本快照的批量复制和删除；普通备份保留策略也不得清理 rollback points/transactions |

## 必须实现的静止条件

1. 网络包下载与验签先完成，然后获取跨进程事务锁。helper 捕获期间 GUI 已退出，避免正常初始化和数据库迁移。
2. IPC、托盘、深链接、自动 OAuth refresh、供应商切换、Skill 和 prompt 写入口必须共用事务门禁。数据库互斥锁不足以覆盖文件写入。
3. `webdav_auto_sync` / `s3_auto_sync` 现有 suppression guard 只抑制新 change signal；worker 的已排队项和已启动 upload 仍可能继续。需等待服务层 sync lock，并对启动新任务再次检查事务门禁。
4. 会话用量扫描和代理请求日志也会写 DB；暂停新任务、等待现有写任务退出。代理排空超时或 Live restore 错误时不启动安装。
5. `cleanup_before_exit()` 目前将错误记日志并继续；事务接口必须返回错误，由协调器处理恢复暂停服务，不能把日志路径当作已停止成功。
6. 用户客户端/编辑器可能独立写配置；逐文件读取前后和整体快照结束时核对版本/哈希。持续变化必须中止捕获。
7. 按规范路径去重并检测重叠 roots、reparse points、硬链接、UNC 和卷类型。原文件缺失的删除资格由库存与写入账本共同证明。

## helper 选型

采用独立 Rust executable 与无 Tauri 依赖的共享 rollback-core crate。helper 自己的 `main` 不调用 GUI library、不启动 Store、不加载/迁移业务 DB；通过只读 SQLite 连接和 Backup API 保持来源 schema。安装器在应用文件替换前将 helper 解出到安装目录外，捕获成功后才允许继续。

候选启动阻挡方案是对原 main exe 添加当前 SID 的临时 deny ExecuteFile DACL。运行 `37572915283` 已证明该 ACL 在历史 Setup 覆盖后保留，且实际 CreateProcess 被拒绝。下一步才接入 helper；原 SDDL、恢复步骤和丢弃该 guard 的时机必须写 journal；helper 路径不能受此 ACL 影响。重启后需恢复执行器并解除 guard，不能只留下一个永久打不开的 exe。产品停机门禁和恢复协调器尚未接入。

## 恢复执行器的当前范围

`windows_resource_restore.rs` 实现精确普通文件恢复：应急库存摘要写入 journal；来源与应急库存逐路径、角色对应；先全量预检再写入；同目录 stage、原 DACL 在写入敏感字节前应用、逐文件进度持久化、原子替换及只读状态恢复。每次续接重新验证材料及已恢复文件，执行器不消费 previous，也不推进全局健康状态。

失败补偿仅在 `Recovering` 执行。恢复原先缺失的文件后，补偿可以凭本次 forward ledger 撤销它；不会凭目录位置删除普通升级期间出现的文件。旧快照缺失、应急快照已有文件时，必须先补应用受管写入账本再授权删除。Skill tree、symlink、业务目录迁移、云同步暂停派生设置仍待对应执行器；当前文件执行器遇到这些库存会在写入任何 live 文件之前中止。它尚未接入 GUI/helper，因此不构成可发布的回退版本。
