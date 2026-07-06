# WASM Agent OS — 架構設計與 TODO(v2,定案 scope)

一個以 wasmtime 為 kernel、單一資料夾為 agent 全世界的微型 Agent OS。
**Scope 定案**:單一 agent/RAG 記憶(host-side SQLite)/Gateway 含 Web 前端/WASM 插件與程式碼執行延後。

核心理念:**沙箱內完整自主,沙箱邊界極簡**。安全邊界 = wasmtime Store 邊界。

---

## 1. 設計目標與非目標

**目標**
- Agent 在自己的資料夾內完整自主(讀寫、自我組織記憶)
- Host 攻擊面最小:無 socket、無 env 繼承、無 host 檔案系統可見性
- Agent 狀態 100% 在資料夾 + 一顆 DB → 可備份、可 git、可遷移、crash-safe
- LLM / embedding API key 永不進沙箱
- RAG 記憶由 agent 每日自主維護
- Gateway 提供 Web 前端:對話、觀察、核准外部資料授權

**非目標(延後)**
- WASM 插件系統、Python/程式碼執行
- 多 agent
- Prompt injection 內容掃描(靠窄邊界,不靠過濾)

---

## 2. 高層架構

```
┌────────────────────────────────────────────────┐
│ Host (Rust binary, "kernel")                   │
│                                                │
│  ┌────────────────┐  ┌───────────────────────┐ │
│  │ Gateway (axum) │  │ Syscall Layer         │ │
│  │  Web UI + API  │  │  llm_call             │ │
│  │  SSE logs      │  │  embed                │ │
│  │  grant 審核     │  │  db_exec (加固後)      │ │
│  └───────┬────────┘  │  sleep_until          │ │
│  ┌───────▼────────┐  │  notify               │ │
│  │ Scheduler      │  │  request_external     │ │
│  │ cron + inbox   │  └──────────┬────────────┘ │
│  │ watcher        │             │              │
│  └───────┬────────┘             │              │
│          │ instantiate          │ host fn      │
│  ┌───────▼─────────────────────▼─────────────┐ │
│  │ wasmtime Store                            │ │
│  │  fuel/epoch limit・memory cap             │ │
│  │  empty env・stdio → log                   │ │
│  │  ┌─────────────────────────────────────┐  │ │
│  │  │ Agent .wasm (user space)            │  │ │
│  │  └─────────────────────────────────────┘  │ │
│  └───────────────┬───────────────────────────┘ │
│                  │ WASI preopen                │
│  ┌───────────────▼───────────────────────────┐ │
│  │ agent-home/  ← agent 看到的 "/"            │ │
│  │  config.toml  memory/  workspace/         │ │
│  │  inbox/  outbox/  logs/                   │ │
│  │  memory/index.db ←(由 kernel 原生 SQLite   │ │
│  │                    代為開啟,見 db_exec)    │ │
│  └───────────────────────────────────────────┘ │
└────────────────────────┬───────────────────────┘
                         │ HTTPS(key 在 host)
                LLM / Embedding API
```

---

## 3. Syscalls(六個)

| syscall | 簽名(概念) | 說明 |
|---|---|---|
| `llm_call` | `(request_json) -> response_json` | host 持 key;token 記帳 + **每日預算硬上限**(config,超額拒絕 + notify);完整 prompt/response 落 transcript log |
| `embed` | `(texts[]) -> {vectors[], model}` | host 打 embedding API;回傳附 model 名;計入每日預算 |
| `db_exec` | `(sql, params) -> rows` | kernel 用**原生 SQLite** 開 `agent-home/memory/index.db`。DB 檔住在 agent 世界內,引擎在 host。**查詢逾時 config 可調,預設 10s** |
| `sleep_until` | `(timestamp)` | agent 宣告下次喚醒,結束本次執行 |
| `notify` | `(message)` | 單向通知人類(gateway 顯示) |
| `request_external` | `(descriptor, reason) -> grant_result` | 單次讀外部資料:掛起 → gateway 人工核准 → **複製**進 `inbox/granted/`。一次性、TTL、audit log、可設白名單免審 |
| `http_fetch` | `(method, url, body?) -> response` | kernel 代發請求。**Denylist 私有網段**(localhost/RFC1918/link-local/metadata)擋 SSRF;全量 egress log(domain+大小);GET 自由放行,**POST/上傳掛起待 gateway 審核**。支援 **secret 佔位符注入**(見 4.8 Credential Vault) |
| `exec_wasm` | `(wasm_path, args, stdin?) -> {stdout, exit_code}` | 執行 agent 自裝的工具:kernel 開**新 Store**,只 preopen workspace、**無網路**、fuel/memory cap。沙箱性質來自 Store,不依賴對二進位的信任 |

**`db_exec` 加固(必做,SQL 在 host 執行)**
- 關閉 `load_extension`
- `sqlite3_set_authorizer` 禁 `ATTACH` / `DETACH`
- 限制危險 `PRAGMA`
- 只允許開啟 agent-home 內的那顆 db 檔

**`request_external` 原則**:複製進來、絕不 preopen 第二個目錄;agent 被 injection 帶歪最多只能「提出請求」。

---

## 4. 元件設計

### 4.1 Kernel
- wasmtime embed,每次喚醒 fresh instantiate(零殘留、crash-safe)
- 資源限制:epoch interruption 逾時(5 min)、memory cap(512MB)
- WASI:`preopen(agent-home, "/")`、env 空、stdio → `logs/`、給 clock/random

### 4.2 Agent(guest .wasm,Rust → wasm32-wasip1)
- 入口 `run(trigger_json)`;trigger:`cron` / `message`(inbox 新訊息)/ `daily_maintenance` / `grant`(外部資料到貨)
- Loop:讀狀態 → RAG 檢索 → 組 prompt → llm_call → 解析行動 → 執行 → 寫回 → sleep_until
- 行動格式(tool-use JSON):`read_file` / `write_file` / `memory_get` / `memory_set` / `notify` / `http_fetch` / `search_web` / `use_skill` / `save_skill` / `schedule_task` / `update_task` / `delete_task` / `request_external` / `done`
  - **`db_query`(原始 SQL action)已拿掉**——使用者觀察到 agent 幾乎不會自己下 SQL,加了 `memory_get`/`memory_set`(`kernel/src/syscalls/memory_kv.rs`,`memory/index.db` 裡一張 `kv(key,value)` 表)取代單一事實的存取需求後,原始 SQL 對 LLM 來說門檻高又少用,乾脆從 action 清單移除。**`db_exec` syscall 本體沒拿掉**——RAG 的 `chunks`/`chunks_fts` 全靠它(`agent/src/memory.rs`),只是不再開放給 LLM 自己下 SQL 這條路
- **`/SOUL.md`**(仿 OpenClaw 的 persona 概念):跟 `config.toml` 一樣是 agent-home 根目錄下一個檔案,純自由格式 markdown(persona/價值觀/語氣),每次組 system prompt 都全文帶入(不像 skills 用 progressive disclosure)。沒有專屬 action——agent 就用既有的 `read_file`/`write_file` 讀寫,人類則走 gateway `/api/soul`(GET/POST,跟 `/api/config` 同款,純文字直通無 schema)+ 前端 Soul 分頁編輯。不存在時視為空,不影響運作。

### 4.3 記憶子系統(agent 自維護 RAG)
```
memory/
  notes/                 # LLM 蒸餾的記憶筆記(markdown,人可讀可改)
  index.db               # SQLite:chunks + FTS5(BM25)+ 向量
  maintenance_report.md
```
- **讀路徑(每次喚醒)**:hybrid 檢索——FTS5 BM25 + 向量 top-N → RRF → top-k 進 prompt
- **寫路徑(每日 `daily_maintenance`)**:
  1. 蒸餾當日 log/對話 → `notes/`
  2. content hash 增量:只 re-chunk + re-embed 變動檔
  3. LLM 自主整理:合併重複、過期降級為摘要、修剪 log
  4. 產出 `maintenance_report.md`(gateway 可看)
- Schema:`chunks(source_path, content_hash, text, embedding BLOB, embed_model)`;`embed_model` 不符 → 自動全庫重嵌
- 向量檢索先用 host 端 sqlite-vec(原生編譯無壓力);筆數少時 BLOB + 暴力 cosine 也行

### 4.4 Gateway(axum + Web 前端)
Kernel space;與 agent 的接觸面僅為檔案系統 + 喚醒,agent 不知其存在。

**API**
| Endpoint | 功能 |
|---|---|
| `POST /api/message` | 寫 inbox + 喚醒 |
| `GET /api/outbox` / `GET /api/status` | 產出/狀態(睡到何時、token 用量) |
| `GET /api/logs`(SSE) | 即時 log 串流 |
| `GET /api/grants`・`POST /api/grants/:id/approve\|deny` | 外部資料授權審核 |
| `GET /api/memory/notes`・`GET /api/memory/report` | 記憶筆記與維護報告瀏覽 |
| `POST /api/pause`・`POST /api/resume` | 暫停/恢復。soft:停止新喚醒,當前執行跑完;hard:epoch interruption 立即中斷(fresh instantiate + auto-commit 保證狀態乾淨) |
| `POST /api/wake` | 手動立即喚醒(開發調試常用) |
| `POST /api/reload` | 熱重載:重讀 config.toml + secrets.toml、清 module precompile cache。agent .wasm 與 workspace 工具本就每次載入,覆蓋檔案即生效 |

**前端**
- 聊天面板(session 化,server 端記歷史)、Log 即時面板(SSE)、Grant 待審卡片、記憶瀏覽器(notes/report/skills)、Scheduler、Config/Secrets 管理頁
- **技術決策翻案(原玩具原則:單檔嵌進 binary,不上 build 系統)**:改成真正前後端分離——`webui/` 是獨立 Vite + Vue SFC 專案(每個分頁一個 component),`vite build` 產出 `webui/dist/`。取捨:換來元件粒度更細、`npm run dev` 熱重載開發體驗;代價是多一個 Node.js 工具鏈依賴、部署變成兩個產物而非單一 binary、多一道 build 步驟。使用者明確要求此翻案。
- **kernel 完全不知道 webui 存在**:一度讓 kernel 用 `tower-http ServeDir` 動態讀 `webui/dist/`,使用者要求連這點耦合都拔掉——kernel 現在純 API server(`/api/*`,`/` 回 404),不 import 任何跟 webui 相關的路徑/概念。兩者怎麼一起跑(dev 用 vite proxy;之後要包裝成單一指令)由**尚未做的獨立 CLI wrapper** 負責,kernel 本身不管。
- 認證:單一 token(secrets.toml 的 `gateway_token`),避免區網亂打

### 4.5 Scheduler
- [x] 單 agent 簡化:tokio loop(`gateway.rs` `scheduler_loop`,每 30s tick 一次),管 next-wake(agent 自己 `sleep_until` 要求的時間到了 → `cron` trigger)+ daily_maintenance cron(今日 `memory/maintenance_reports/<date>.md` 缺了就補,15 分鐘重試一次直到成功)。inbox watcher 不需要——沒有非同步 inbox 概念,`message` trigger 由 `/api/message` 同步處理
- 兩種自發喚醒(`daily_maintenance`/`cron`)都不帶 chat `history`,結構上就是全新 session(`agent_loop.rs` 只有拿到 `trigger.history` 才會接續對話)——不會污染正在進行的聊天 session,已用真跑驗證(scheduled run 跑完後 `logs/session.json` 完全沒被動到)
- 每次自發喚醒都留紀錄:`logs/scheduled_runs/<ts>-<type>.json`(trigger + outcome),`GET /api/scheduler/runs` 可查,前端 SchedulerPanel 列表顯示
- [x] **使用者自訂 scheduled task**(人或 agent 都能新增):格式就是「cron 時間 + 事件資料位置」——`scheduler/<id>.json`(一檔一 task,跟 `memory/skills/<name>.md` 同款,agent 也能直接 read_file/write_file),欄位 `{id, cron, data_path, description, enabled, created_at, last_run}`
  - `cron`:自寫 5 欄位 matcher(`kernel/src/cron.rs`,`*`/數字/逗號列表/`*/step`,UTC,無時區),沒拉 `cron` crate——跟 `logs::today_utc` 同一套 Howard Hinnant 手刻日期算法,不引入 chrono
  - `scheduler_loop` 每 tick 檢查所有 enabled task,cron 對上當下分鐘就發 `scheduled_task` trigger(帶 `task_id`+`data_path`,一樣不帶 history,全新 session);同一分鐘只發一次(`last_run` 同分鐘就跳過),避免 30s tick 在同一分鐘內重複觸發
  - CRUD 兩條路都通:前端 SchedulerPanel(新增/編輯/刪除/enable 開關,`GET/POST /api/scheduler/tasks`、`PUT/DELETE /api/scheduler/tasks/{id}`);聊天對話裡 agent 自己用 `schedule_task`/`update_task`/`delete_task` action(host 端 `kernel/src/syscalls/schedule_task.rs`)——agent 自己能設「每天早上提醒我」這種任務,不用人手動去前端建
  - 已用真跑驗證:建一個 `* * * * *` 的 task 指向自寫的 `/workspace/tasks/ping.md`,scheduler tick 真的抓到、真的呼叫 Kimi 跑完、`last_run` 更新;PUT 停用、DELETE 刪除、bad cron(欄位數不對)被拒絕,四條路都測過
- [x] 執行中收到新訊息 → 排隊,本次跑完再喚醒:`AppState.run_lock`(`tokio::sync::Mutex<()>`)在 `run_trigger` 裡整個 run 期間鎖著——chat/`/api/wake`/session compact/三種 scheduler 自發喚醒全部共用同一個 `run_trigger`,天生就排隊,不會有兩個 wasm 同時跑撞 `memory/index.db`

### 4.6 網路政策(egress 控制模組化,config 可調)
威脅模型不是「agent 是壞人」,是「agent 被網頁內容騙」(prompt injection → exfiltration)與 SSRF。

**固定護欄(不可關)**
1. Denylist 私有網段:localhost、RFC1918、link-local、169.254.169.254(DNS 解析後查 IP 落點,防 rebinding)
2. 全量 egress log:記**完整 URL** + bytes,gateway 可查
3. POST/上傳審核佇列

**GET egress 模式(config 切換,預設 `open`)**
```toml
[network]
get_mode = "open"        # open | tofu | allowlist
url_max_len = 2048        # 防 query string 夾帶大量資料
daily_request_cap = 500

[ratelimit]               # token bucket,host 端
llm_per_min = 10
http_per_min = 30
http_per_domain_per_min = 10   # 對外禮貌,防同站連打被 ban IP
```

**撞限語義(簡單版)**:block 等待最多 **3s**,bucket 補上就過;仍無配額回 `rate_limited` + retry_after。3s 相對 5 min epoch 配額可忽略,**不需 epoch 補償**。速率持續打頂 → notify(失控迴圈的早期警報)。
- `open`:GET 自由(預設,好用優先)
- `tofu`:新 domain 首次使用需 gateway 核准,之後永久放行——想收資料必用新 domain,必撞審核
- `allowlist`:僅名單內 domain(最嚴)

已知取捨:`open` 模式下 GET query string 是 exfiltration 通道,url_max_len + 完整 URL log 為緩解;在意時切 `tofu`。

### 4.7 工具自動安裝(agent 自建 toolbox)——**已嘗試,已回退**
「安裝」= 把 .wasm 放進 workspace 外的 `tools/`。二進位信任問題由 Store 解決:惡意工具最壞搞亂 workspace,無網路、出不了 agent-home。

**流程(設計,未實作)**:agent 判斷需求 → http_fetch 搜尋/下載(GET)→ kernel 輕量驗證(合法 wasm module、size cap)→ 落地 `tools/` + 寫 `lockfile.json`(name/source_url/sha256/installed_at)→ `exec_wasm` 使用。

- **Lockfile**:gateway 可審計裝了什麼;agent 遷移時 kernel 照 lockfile 還原工具;hash 可比對官方 release
- **工具輸出視為不可信輸入**(stdout 進 LLM context = injection 通道,agent prompt 中註明)
- **磁碟配額**:agent-home 總量上限(如 2GB),超過拒寫 + notify
- 現成生態:uutils(coreutils)、jq、ripgrep、wasm-git 等皆有 WASI build
- **curl 類無意義**:網路是能力不是程式,exec_wasm 的 Store 無 socket,裝了也發不出封包——網路永遠只能經 kernel 的 http_fetch

**[~] 實際做過又回退的紀錄**:為了讓 agent 能列目錄/刪檔案,一度把 `exec_wasm` syscall 真正接上(`agent_loop.rs` 補 `Some("exec_wasm")` arm)、手刻 `tools/unix`(wasm32-wasip2 component,`ls`/`cat`/`mkdir`/`rm`)、`wasm.toml` manifest 動態組進 system prompt、`tools/` 獨立於 `workspace/` 之外的目錄分離(修掉「工具能刪自己」的洞)——全部真的建好、測過、真跑驗證過。

事後反省砍掉重做:`exec_wasm`/wasmtime component hosting 這整套複雜度存在的唯一理由是「要跑不信任的程式碼」,但「agent 自己上網抓現成工具」這個場景本身已經評估放棄(抓來源不明、且多半是 wasip2 跟舊 wasip1 沙盒不相容)——放棄之後,`tools/unix` 是我們自己寫、自己信任的程式碼,根本不需要沙盒。更進一步發現 `read_file`/`write_file` 本來就不是 host syscall,是 `agent_loop.rs` 直接用 guest 自己的 `std::fs`(agent.wasm 本來就 preopen 整個 agent_home)——照這個先例,`list_dir`/`make_dir`/`delete_path` 一樣直接加 3 個 action arm 用 guest-side `std::fs` 就好,不用多繞 wasmtime-in-wasmtime 這層。

**最終方案**:`exec_wasm` syscall、`kernel/src/syscalls/exec_wasm.rs`、`tools/unix` crate、`agent/src/tools.rs`、`wasm.toml` manifest 全部刪除。`list_dir`/`make_dir`/`delete_path` 三個 action 直接在 `agent_loop.rs` 用 `std::fs`,跟 `read_file`/`write_file` 同款寫法。這條路留著記錄是因為「wasmtime component model host 一個真正的 wasip2 tool 跑得通」這個技術驗證本身是有效資訊——之後如果真的要跑不信任的第三方程式碼,這套做法已經驗證過可行,不用重新摸索;只是現在沒有這個需求,先不留著閒置的複雜度。

### 4.8 Credential Vault(secret 永不進沙箱)
不新增 syscall,做在 `http_fetch` 邊界:

1. Kernel 端 `secrets.toml`(**存於 agent-home 外**),gateway 管理
2. 每個 secret 綁定 domain:`github_token → api.github.com`
3. Agent 用佔位符:`Authorization: Bearer {{secret:github_token}}`
4. Kernel 驗證 secret ↔ domain 綁定 → 通過才注入真值發出;不符則拒絕 + notify

**性質**:agent 從頭到尾沒看過明文,無物可洩——比 egress leak scanning 更強的保證。Injection 最多騙 agent「把 token 用在 evil.com」,domain 綁定直接擋下。Response 寫回 log 前掃一次 secret 值以防 API 回聲(唯一需要掃描的點)。

---

## 5. 關鍵取捨(已定案)

| 決策 | 選擇 | 理由 |
|---|---|---|
| SQLite 位置 | host 原生 + `db_exec` syscall | WASI 無共享記憶體/完整檔案鎖,WAL 不可用;host 端 FTS5/sqlite-vec 全原生。代價:必須做 authorizer 加固 |
| 程式碼執行 | 延後 | 先看 agent 純靠檔案 + llm_call 能活成什麼樣;之後要加,走「python.wasm 作為插件」或 rootless container 再議 |
| wasip1 + JSON | 是 | 避開 Component Model 前期成本;曾為 `exec_wasm` 工具沙盒另外加過 wasip2 component 支援,後來連 `exec_wasm` 本身都回退掉了(見 4.7)——目前全專案只有主 agent 這一個 wasmtime Store |
| fresh instantiate | 是 | crash-safe、零殘留 |
| 前端 | ~~無 build 系統單頁~~ → 獨立 Vite+Vue 專案(`webui/`),真前後端分離 | 原本圖雜務最小化;使用者後來明確要求前後端分離、元件粒度更細,翻案換取這個 |

---

## 6. TODO List

### Phase 0 — 骨架(~1 天)
- [x] repo:`kernel/`、`agent/`、`agent-home/`
- [x] wasmtime 載入 hello-world .wasm 並執行
- [x] guest Rust → wasm32-wasip1 build 通
- [x] preopen 打通:guest 在 `/` 讀寫 → 落在 `agent-home/`
- [x] 隔離測試:`..`、絕對路徑、symlink 逃逸全失敗(寫成測試)

### Phase 1 — Syscalls(~3 天)
- [x] 六個 syscall 的 JSON schema(request/response/error)— 統一 envelope,見 `kernel/src/abi.rs`
- [x] `llm_call`(reqwest 打 API,key 從 host env)+ token 記帳 → `logs/usage.jsonl`
- [x] **每日預算硬上限**:config 設額度,llm_call/embed 超額直接拒絕 + notify
- [x] **llm_call rate limiter**(token bucket):block ≤3s → 仍無配額回 rate_limited;打頂 notify
- [x] **Transcript log**:每次 llm_call 完整 prompt/response 落 `logs/transcripts/`
- [x] `embed`
- [x] `db_exec`:原生 SQLite 開 `memory/index.db` + **authorizer 加固**(禁 ATTACH/load_extension/危險 PRAGMA)+ **查詢逾時(config,預設 10s)**
- [x] `notify` / `sleep_until`
- [x] 資源限制:epoch 逾時、memory cap、env 清空、stdio → log
- [x] 安全測試:db_exec 嘗試 ATTACH `/etc/passwd` → 被拒

### Phase 2 — Agent Loop(~3 天)
- [x] 讀 `config.toml` + 記憶 → 組 system prompt
- [x] 行動 JSON 解析與多輪執行迴圈
- [x] 記憶寫入:結束前更新 `notes/`
- [x] **Auto-commit**:每次執行結束 kernel 對 agent-home 做 git commit(大腦時光機;搞壞記憶可 rollback)
- [x] 端到端:給任務,觀察跨多次喚醒的行為

### Phase 3 — RAG 記憶(~3-4 天)
- [x] chunks schema + FTS5 虛擬表(guest 端 `ensure_schema`,contentless FTS5,見 `agent/src/memory.rs`)
- [x] chunking:markdown-aware(標題邊界 + overlap)
- [x] hybrid 檢索:BM25 + 向量(BLOB/TEXT cosine,筆數少暴力法)→ RRF → top-k
- [x] agent loop 接上讀路徑
- [x] `daily_maintenance` cron:蒸餾 → hash 增量重嵌 → LLM 整理 → report(cron 排程本身是 Phase4 scheduler 的事,trigger 處理已就緒)
- [x] embed_model 變更 → 全庫重嵌
- [x] 模擬多天連跑,驗證記憶累積與檢索命中

### Phase 4 — Gateway + 前端(~3-4 天)
- [x] axum:message / status / logs(SSE)(outbox 概念用 `/api/message` 同步回傳 result 取代,還沒有非同步 outbox 佇列)
- [ ] grant 流程:`request_external` 掛起 → 審核 API → 複製進 `inbox/granted/` → 喚醒 → audit log(**使用者明確決定先不做**——`http_fetch` 自己的 grant queue(tofu/write 審核)已經覆蓋大部分「網路動作要人審」的需求,`request_external` 這條專用管道暫緩)
- [ ] grant TTL + 單次性 + config 白名單免審(同上,延後)
- [x] 前端單頁:聊天、log 面板、記憶瀏覽器(無 grant 卡片,功能還沒做)
- [x] 控制面板部分:wake now(`/api/wake`)。**pause(soft/hard)/resume 使用者明確決定不做**——4.5 有 scheduler loop 也有 `run_lock` 排隊了,但沒有「暫停」開關本身;`/api/abort` 已經涵蓋「中斷正在跑的這一次」的實際需求,soft/hard pause 這種更複雜的排程層開關暫不追
- [x](簡化掉)Kernel config 不需要 `ArcSwap`:`Config::load` 本來就每次喚醒重新讀檔,沒有跨喚醒快取,寫入 `/api/config` 下次喚醒自動生效,不需要熱重載機制
- [ ] hard stop 測試:依賴 pause/resume,不做了故此測試也不用補
- [x] token 認證(單一 bearer token,`/` 首頁例外;SSE 因瀏覽器 EventSource 限制額外接受 `?token=` query)
- [x] 前端嵌進 binary(`include_str!`,單一 html 檔,無 build 系統)
- [x] **新增(非原清單)**:`POST /api/config` 寫入前先驗證 TOML 能 parse 進 `Config` struct;`POST /api/secrets` 只寫不讀——回應只給 vault 內的 key 名稱清單,沒有任何 endpoint 能讀出 secret 值

### Phase 5 — 網路與工具自動安裝(~2-3 天)
- [x] `http_fetch`:reqwest 代發;DNS 解析後檢查 IP 落點(`resolve()` 釘住,防 rebinding TOCTOU)
- [x] Denylist:localhost / RFC1918 / link-local / metadata endpoint
- [x] Egress log(**完整 URL** + bytes)→ gateway 可查(`GET /api/egress`)
- [x] GET 模式模組:`open`(預設)/ `tofu` / `allowlist`,config 切換
- [x] `url_max_len` + `daily_request_cap` 限制
- [x] **http rate limiter**:全域 + per-domain token bucket(語義同 llm_call)
- [x] POST/上傳審核佇列(`grants.rs` + `GET/POST /api/grants`,tofu 新 domain 共用同一佇列)
- [x] `exec_wasm`:新 Store、只 preopen workspace、無網路、fuel/memory cap
- [ ] 安裝驗證:wasm module 合法性 + size cap;寫入 lockfile.json(**使用者明確決定不做**——原本設想走 wapm 生態自動裝工具,現在 wapm 已不是活躍選項,agent 自動安裝工具這條路線本身不追了;exec_wasm 本體單獨可用不受影響)
- [ ] agent-home 磁碟配額 + 超額 notify(延後——WASI preopened dir 沒有攔截點可即時擋寫入,需要更深的 wasmtime-wasi hook 才能做到「拒寫」而非事後檢查)
- [x] gateway 前端加 egress log 面板(`webui/src/components/EgressPanel.vue`,`GET /api/egress`)。已安裝工具列表(lockfile)面板延後——lockfile 本體(上一行)還沒做,沒東西可列
- [ ] Credential vault:secrets.toml(agent-home 外)+ gateway 管理頁(現有 vault 只支援 llm/embed 用的 `{secrets.x}`,domain 綁定機制延後)
- [ ] 佔位符注入:`{{secret:name}}` 展開 + domain 綁定驗證(延後)
- [x] Response 落 log 前掃 secret 值(防 API 回聲)——`http_fetch` 的 `redact_secrets` 已做,對象是現有 vault 的值
- [ ] 測試:secret 用在未綁定 domain → 拒絕(domain 綁定機制延後,無法測)
- [x] 測試:`exec_wasm` 跑通(用純 WASI 測試二進位而非真裝 jq.wasm)、工具讀不到 `memory/`;`http_fetch` 打 `192.168.x.x`/`127.0.0.1`/`169.254.169.254` 全被拒

### 未來糖果罐(延後)
- [ ] **Agent 互通(A2A,actor model)**:設計已定——新 syscall `send_agent(target, msg)`,kernel **複製**訊息至對方 `inbox/from-<sender>/` 並喚醒;不共享任何目錄,Store 間零接觸;通訊拓撲在 kernel config 逐條宣告(capability),未宣告組合拒絕;訊息全經 kernel = 全量 A2A log,gateway 可視化對話圖。支援監督者模式、互相 review 等玩法;新 agent = 新資料夾 + 一行拓撲
- [ ] 多 agent 基礎(每 agent 一個 Store + 資料夾,scheduler 泛化)
- [ ] wasip2 / Component Model 遷移
- [x] **Discord adapter**(接在 gateway 上,不動 kernel——`kernel/src/discord.rs`,serenity crate 跑 Gateway websocket):只回 DM 或 @mention(避免頻道裡隨便講話都觸發);每個 DM/頻道各自一份 session(`discord-dm-<user>`/`discord-channel-<channel>`,存在 `logs/chat_sessions/<key>/`),不跟 webui 的 `webui` session 混;RAG/SOUL/skills/scheduled tasks 全域共用,只有原始對話串分開。沒設 `discord_bot_token` secret 就整個不連,gateway 照常運作。連帶把 session 儲存從單一寫死路徑改成 keyed(`session_dir(agent_home, key)`),為多 channel 鋪路
  - [x] session compact/reset 泛化成 keyed(`gateway.rs` `compact_session_key`/`reset_session_key`,原本寫死 `"webui"`),webui 兩顆按鈕跟 Discord `!compact`/`!reset` 指令共用同一套;Discord 沒有前端按鈕可按,另外加一個 auto-compact:單一 session 的 `context_tokens` 超過 `config.toml` `[chat] auto_compact_tokens`(預設 50000)門檻,下次那個 session 一有新訊息就在背景自動 compact,不擋當次回覆。`session_watch_loop` 原本 `turns.len() <= last` 沒處理 session 被 compact/reset 縮短的情況,下次成長超過舊 `last` 會 slice 越界 panic——已修成偵測到變短就重新 baseline
- [ ] Telegram adapter(接在 gateway 上,不動 kernel)——跟上面 Discord 同一套改法,概念已驗證過
- [ ] python.wasm 作為標準工具(module precompile cache)——層次一自主開發:agent 寫 Python、exec_wasm 跑、迭代

### 里程碑
- **M1**(P1 完):guest 經 syscall 完成一次 LLM 對話 + DB 寫入
- **M2**(P2 完):agent 自主多輪任務 + 管理自己的筆記
- **M3**(P3 完):每日維護跑起來,記憶檢索命中
- **M4**(P4 完):瀏覽器裡跟 agent 對話、看它的腦、核准它的請求
- **M5**(P5 完):agent 自由上網查資料、自己下載工具並用 exec_wasm 跑起來

---

## 7. 安全檢查清單
- [x] preopen 逃逸測試通過(traversal / symlink)
- [x] 沙箱內 env 為空
- [ ] API key 不出現在任何 agent 可讀路徑或 log
- [x] db_exec:ATTACH / load_extension / 危險 PRAGMA 全被拒
- [x] 逾時與記憶體上限實測觸發
- [ ] grant:只複製、無額外 preopen;audit log 完整(請求理由/核准人/時間)(request_external 本體延後,grant 佇列機制已用於 http_fetch)
- [x] gateway token 認證生效
- [ ] agent-home 整個刪除 → host 不 crash
- [x] http_fetch:私網段全被拒(localhost/RFC1918/link-local/metadata);egress log 無遺漏(DNS rebinding 情境靠 resolve()-then-pin 結構性防護,未特別造一個會 rebind 的測試網域)
- [x] POST 未經核准不會發出:實測 queue → approve → 同一請求重放才真的發出
- [x] exec_wasm 的 Store:看不到 memory/(只有 workspace)實測;無網路是 WASI p1 command module 結構性保證(沒有 socket API),未額外造測試
- [ ] 磁碟配額實測觸發(功能本身延後)
- [ ] vault:agent 全世界內 grep 不到任何 secret 明文;domain 綁定不符即拒(domain 綁定延後)
- [ ] 預算硬上限實測:超額後 llm_call 被拒且 notify
- [x] db_exec 逾時實測:慢查詢被砍,kernel 不卡死
- [x] url_max_len / daily_request_cap 實測觸發
- [ ] rate limiter 實測:模擬失控迴圈 → 3s 內節流放行或收到 rate_limited,且 notify 觸發
- [x] auto-commit 可 rollback:手動污染 notes/ 後 git checkout 還原