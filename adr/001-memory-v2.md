# ebina — 記憶與技能子系統設計 v2

**三層瀑布式蒸餾 + 檢索式技能。**
核心:資訊單向沉澱,每層只吃下層的**產物**而非原始資料 → context 成本恆定,時間本身成為品質過濾器。

---

## 1. 全景:三層瀑布

```
raw   transcripts / 對話 / syscall 結果 / 任務產出 / workspace/memory/ 短期筆記
  │
  ├─ 1h  快維護 ────→ 直接寫入/更新 notes/ + hourly report(給 6h 的來源指標)
  │      吃:近 1h raw(含 workspace/memory/ 掃描,見下方附註)
  │      做:簡蒸餾,快速直接落地寫進 notes/(只顧自己這 1h 的 delta,不回看整個
  │          notes/ 做全域整合)。無新 raw → short-circuit 跳過,不呼叫 LLM
  │
  ├─ 6h  中維護 ────→ 6h summary(經歷)
  │      吃:近 6 份 1h report(主要輸入,不做 notes/ 全檔掃描——report 已經是 1h 層
  │          蒸餾過的產物,足夠判斷哪裡碎片化/矛盾;真的拿不準才 read_file 個別
  │          note 回查,不是例行全掃)
  │      做:**整併,不是第一次寫入**——好幾次 1h pass 各自快速寫出來的東西容易碎片化
  │          /互相矛盾,6h 負責合併重複、解決衝突、過時降級,讓 notes/ 保持乾淨;待辦
  │          升級(見 §4);技能品質閘門(見 002 §3——掛在這層,不是 24h,兩者互不
  │          依賴,6h 頻率也讓沒用的技能不會晾一整天才被複查);動完 notes/ 收尾前
  │          呼叫一次 `reindex_all_notes`(見下方附註跟 §5)
  │
  └─ 24h core 蒸餾 → core.md(常識快取)
         吃:近 4 份 6h summary(主要輸入)+ 現有 core.md 本體(判斷哪些該降級/覆蓋,見
         §2.3),同理不做 notes/ 全檔掃描
         做:notes↔core 晉升/降級;同樣動完notes/ 收尾前呼叫一次 `reindex_all_notes`

三層產物:
  core.md    常識快取層  無條件全量注入(~2K 上限),記憶蒸餾的產物,不是身份設定
  notes/     經歷層      無上限成長,走檢索
  index.db   檢索層      notes chunks + skill_chunks(獨立 table,不混用),hybrid search
```

**路徑**:`core.md` 放頂層 `/core.md`(跟 `/SOUL.md` 同一層,兩者都是無條件注入的檔案);
`memory/` 底下**只放 `notes/` 跟 `index.db`**——`maintenance_reports/`(1h/6h/24h 的過程
產物,見下方)跟 `skills/`(見 002)都搬到頂層獨立目錄(`/maintenance_reports/`、
`/skills/`,跟 `/workspace/`、`/logs/` 同一層),不塞進 `memory/`。理由:`memory/` 該只
裝「curated 記憶本體」,maintenance_reports 是蒸餾過程留下的中繼產物(性質比較接近
`/logs/`),skills 是完全獨立子系統(見 002 §1),兩者混進 `memory/` 容易誤導成「這些也
走記憶蒸餾那套權威/新鮮度規則」,其實不是。

**`memory/notes/<date>/log.md`(每日 run log)也要搬**:查過 `agent_loop.rs:1300-1310`
確認,這份逐日累積的原始 run 記錄(每次喚醒的 trigger/summary verbatim)本來就**不進索引**
——`reindex_all_notes` 現有實作明確跳過子目錄(`if path.is_dir() { continue; }`),只掃
`memory/notes/` 頂層的 `.md` 檔案,理由文件裡寫得很清楚:log 是未經審查的逐字稿,embed
進去會讓 hybrid_search 撈到「使用者當初講錯、後來被修正」的舊引言,污染檢索。既然機制上
從來不索引,物理位置也不該混在 `memory/notes/` 底下跟真正 curated 的 topic notes 放一起
——搬到 `/logs/`(跟 transcripts/scheduled_runs 同一層,性質本來就更接近那邊),
`memory/notes/` 之後變成真正的純平面目錄(只有 topic notes,沒有日期子目錄),
`reindex_all_notes` 那條 `is_dir` 跳過邏輯也可以直接拿掉,不用留著防一個已經不存在的情境

**跟 `/SOUL.md` 的關係**:SOUL.md 是人設(角色/語氣/邊界),人手動維護,每次全文注入;
core.md 是記憶蒸餾出來的「常識快取」,24h 蒸餾自動維護。兩者都無條件注入,但職責不重疊
——core.md 不放身份/語氣類內容(那是 SOUL.md 的事),只放蒸餾出來、值得長駐的**事實**。

**快維護週期維持 1h,不改**:目前 1h 已有 short-circuit(無新素材不叫 LLM),空轉成本
已經接近零,拉長週期換不到什麼,還會讓「需關注」項目卡更久沒被複審。若之後真的需要調,
開一個 config 項而非寫死,但現階段不需要。

**`workspace/memory/`(短期筆記暫存區)併入這份設計**:目前 daily_maintenance 除了看
raw log delta,還會額外 `list_dir`/`read_file` `/workspace/memory/`——那裡是短期筆記/
提醒的指定存放位置(fact/todo 分流:fact 蒸餾進 notes/、todo 留在原地並寫進 report 的
「需關注」段、蒸餾完的筆記 `delete_path`)。1h 層繼續做這件事,不是被取代。

**誰改 notes/ 誰就自己 reindex,不是只有 1h 才做**:§5 的修法(反向比對 DB `source_path`
跟磁碟現存檔案,清掉懸空的 `chunks`/`chunks_fts` 列)是直接改 `reindex_all_notes`——這個
函式本身不專屬 1h,任何一層動完 `memory/notes/` 之後,自己在同一次 run 收尾前呼叫一次
`reindex_all_notes`(增量重嵌 + 反向懸空清除都在裡面),不要留給「反正下次 1h 會跑」這
種隱含假設。1h 蒸餾完呼叫一次、6h 整併完呼叫一次、24h 執行 core 改動順便呼叫一次——同一
個函式,三層各自收尾時都調用,不是誰專屬。這樣搜尋結果永遠反映當下磁碟狀態,沒有任何
一層改完到下次被重新索引之間的過期窗口。

**上下層錯開 fire 分鐘,不用互查 flag(這點也影響現有已上線 code,不只未來 24h 層)**:
三層目前都釘死同一個整點分鐘(`:13`)fire,加上 `scheduler_loop` 現在用 `tokio::spawn`
背景跑(不 block tick),同一個 tick 裡 6h 條件可能在 1h 那份 report 都還沒寫完時就先
開始讀。修法不用共享狀態的 running flag,單純把三層的 fire 分鐘錯開(1h `:13`、6h
`:18`、24h `:23`),留 5 分鐘 buffer 給下層正常寫完,lock-free:
- 常見情況:5 分鐘 buffer 夠用(1h daily_maintenance short-circuit 時幾乎瞬間完成,真
  的有事要處理也通常 1 分鐘內結束),下層寫完換上層讀,沒有 race
- 極端情況(1h 那次卡到 retry/circuit breaker 冷卻,拖超過 5 分鐘 buffer)——退回既有的
  checkpoint 自癒:`has_new_maintenance_reports` 用 mtime 比對上次消化到哪裡,被呼叫時
  就從上次記錄的狀態繼續往下走,不會漏資訊,只是那份報告要等下一輪 6h/24h 才被消化進
  去,不是資料遺失,這個後果本來就是現有系統一直可接受的行為
不需要引入 `daily_maintenance_running`/`maintenance_summary_running` 這類共享 flag,
也不用「忙就跳過這個 tick、下個 30s 再檢查」的額外邏輯

**為何 core 放在 24h**:它吃的是被兩層過濾過的東西——一時興起活不過 6h 整合,活過四份 6h summary 的才有資格競爭 core 席位。**時間即品質過濾器**,不需靠 prompt 叫 LLM「謹慎一點」;架構本身防住了 core 躁動。

**保真度衰減對策**:各層 summary 保留**來源指標**(wake_id / note 路徑)。典型情境:24h
蒸餾讀到一則 6h summary 寫得很精簡(摘要的摘要,例如「頭像提醒已處理」),沒講細節,
拿不準這件事該不該變成 core 常駐內容時,用 provenance 存的 note 路徑 `read_file` 回查
一次原始筆記,而不是憑那句壓縮過的摘要瞎猜。統一用 `read_file`(讀路徑指到的檔案現在
的內容),不用 `db_exec` 查 `chunks` 表——provenance 存的本來就是路徑不是 chunk id,
而且 chunks 表裡的 text 是 embed 當下的舊快照,檔案如果被 6h 整併過,兩者可能已經對
不上,`read_file` 才是真正的「現在的原文」。

---

## 2. Core 記憶(常識層)

### 2.1 判準
「任何一次隨機喚醒,不知道這件事會不會讓表現變差?」→ 會,才進 core。
存**常識**,不存**經歷**;存**記憶蒸餾出的事實**,不存**身份/語氣**(那是 SOUL.md 的事,
core.md 不重複)。

### 2.2 內容兩類
1. **當前重心**:進行中的 2-3 件事各一行(唯一會頻繁流動的部分)
2. **agent 職責**:每日任務、該盯的目標——蒸餾出「持續有效」的那種,不是單次待辦

反例(住 notes/,不進 core):「上週三聊過 Kafka rebalance」「某 repo 上週出 2.1」——情節記憶。
反例(住 SOUL.md,不進 core):角色設定、技術棧偏好、溝通語氣、輸出格式邊界——這些是人設,
不是蒸餾產物,core.md 不重複維護。

### 2.3 機制
- **上限 ~2K 字元**:塔頂稀缺席位,逼出取捨品質。超限拒寫,但不當場算失敗——`write_file`
  的錯誤訊息連字數一起回給 LLM,同一個 run 裡讓它自己重新取捨、再寫一次,最多 2-3 次
  (同一個 run 內的迴圈,不是跨 run 的 retry backoff)。**還是超限,trigger_note 要明講
  這種情況下 `done` 的 `summary` 得以 `"run aborted: "` 開頭**——查過 `agent_loop.rs:188`
  確認,`"run aborted"` 這個字串現在只有 host 端 `llm_call` 真的失敗時才會自動產生,LLM
  自己正常 `done` 完成不會寫出這個字串,`outcome_aborted`(`gateway.rs`)那個機械檢查抓
  不到「LLM 自己放棄了」這種情況。不明講這條 prompt 慣例,連續塞不進 2K 的 run 會被當成
  正常完成,core.md 沒真的改掉也不會觸發現有失敗重試那套,沒人知道
- **讀**:每次喚醒無條件全量注入(~500 token 常駐)
- **寫**:只在 24h core 蒸餾,沒有例外路徑——身份級大事一樣走 raw→1h→6h→24h 全程,最慢
  接近一天才真正進 core。這是刻意的取捨:換掉「當場改」等於重新打開一個已經堵掉的洞
  (即時聊天寫入現在明確限定只能碰 `/workspace/`,不能碰 curated 記憶,見 PROJECT.md §4.2
  的既有安全邊界),而且當場改 core 會直接打斷 prompt cache 前綴穩定性,牴觸 §6 的 cache
  目標。真正重要的事在那之前一樣看得到——它會進 notes/,一樣被檢索得到,只是還沒進 core
  常駐
- **冷啟動其實不是問題,之前算錯了**:4 份 6h summary 間隔是 4×6h=24h=**一天**,不是
  「等 4 天」。24h 層第一次照自己排程該跑的時候(啟動滿 24h),6h 層早就已經跑滿 4 次,
  東西自然就在那——連特例都不用寫,一次性回填完全沒必要,**不做**。而且
  `maintenance_summary` 這次 session 已經上線在跑,`/maintenance_reports/summary/*.md`
  (遷移前是 `memory/maintenance_reports/summary/`,見上方路徑附註)已經累積好幾份真實
  報告,24h 層一旦真的實作出來,一啟動甚至立刻就有得吃
- **權威**:core 是 single source of truth。**權威給經過審查的那層,不給更新快的那層**
- **是增量編輯,不是每次憑空重推導全文**:24h 寫 core.md 前會先讀現有 core.md 本體
  (判斷哪些該降級/覆蓋)+ 4 份新 6h summary,不是只看 6h summary 就重寫一份全新內容。
  這代表 staleness 修正**不是機制保證**,是 LLM 那次讀完新舊資料後主動判斷「這條過時
  了,拿掉」——沒有強制逐條檢查的步驟。原本的矛盾佇列就是在補這個洞(機械記錄+6h裁決),
  拿掉之後這是刻意接受的風險,不是遺漏(見 §2.4、§5)

### 2.4 權威 vs 新鮮度
notes 更新快(1h 一次,直接寫入)、core 更新慢(24h)——但這是分工不是缺陷:
- notes = **原始素材**(快、雜、未審查,含猜測與後來被推翻的東西)
- core = **經過審查**(慢正是品質來源;防污染的最後一道閘)

若以 notes 為準,injection 寫進 notes 的假事實就能覆蓋 core。staleness 問題不靠翻轉權威解決——24h 每次寫 core.md 前會重讀現有 core.md 本體 + 最新 4 份 6h summary,由 LLM 主動判斷哪些該降級/覆蓋(見 §2.3),沒有例外路徑可以繞過這個審查節奏提早生效;但這代表 staleness 修正依賴 24h 那次判斷夠不夠準,不是機制強制保證(拿掉矛盾佇列後刻意接受的取捨)。

---

## 3. 技能(程序記憶)

拆到獨立文件 [002-skill-v2.md](002-skill-v2.md) 了——技能是自成一格的子系統(不吃
1h/6h report),混在同一份文件裡討論太雜。品質閘門掛在本文件 6h 中維護層
執行(§1 diagram 已經標,跟 core/notes 的 24h 晉升降級是分開的兩層,不依賴彼此),
索引(`skill_chunks`,獨立 table)是 §1 檢索層的一部分,除此之外細節都在 002。

---

## 4. 待辦升級(沿用現有機制,掛在 6h 中維護)
待辦升級處理的是**沒人處理的待辦事項**(只有人類能做的動作,例如「幫 bot 換頭像」)——
現行系統已經有這個機制,且真的觸發驗證過,這版不能默默丟掉:

- 1h 層把待辦留在 `/workspace/memory/` 原地,寫進自己那份 report 的「需關注」段(不進
  notes/,不是事實)
- 6h 中維護讀過去 6 份 1h report,同一項「需關注」連續出現 3 次以上都沒變化 → 主動
  `chat_send` 通知人,不再只是被動重複記錄

---

## 5. 去重(core vs index.db)

同一事實在 core 與 notes 並存是**良性重複**:core 是常識版(無條件在場),notes 是細節版(帶 core 沒有的具體資訊)。

兩條規則解掉毒性:
1. **core.md 不進向量索引**(只索引 notes/)→ 檢索不會浪費席位撈回 core 已說過的事
2. **core 由 24h 週期性覆核**(讀舊 core.md 本體 + 新 6h summary,由 LLM 判斷去留,見
   §2.3),不靠靜態優先級盲目覆蓋——但這是 LLM 主動判斷,不是機制強制逐條檢查,矛盾佇列
   拿掉後這個風險是刻意接受的取捨,不是遺漏

**整併/刪除要連 index 一起清(現有 code 已受影響,不只未來設計)**:查過
`agent_loop.rs`/`memory.rs`——`delete_path` action 只 `fs::remove_file`,完全沒碰
`chunks`/`chunks_fts`;`reindex_all_notes` 只走「現在磁碟上還存在的檔案」重新 embed,
沒有反向偵測「曾經索引過、現在檔案已經不在了」去清掉對應列。等於:1h/6h 層合併重複
筆記、刪掉舊檔的時候(§1 已經說這是常態動作),那個舊檔的 chunk 會永遠留在
`chunks`/`chunks_fts` 裡變成懸空索引——檢索撈得到內容,但 `source_path` 指向的檔案
已經不存在,`read_file` 回查原文(§1「保真度衰減對策」那段)會直接失敗。修法:
`reindex_all_notes`(或另開一個 sweep)要反向比對 DB 裡的 `source_path` 集合跟磁碟上
實際存在的檔案,對不上的整批 `DELETE FROM chunks/chunks_fts WHERE source_path = ...`。
這是現有系統的洞,建議跟 v2 的其他改動一起補,不用等 24h 層才修。

**三層各自收尾都呼叫一次,會不會變重?不會**:
- forward 增量那段——`same_as_indexed` 對每個檔案是一次本地 `SELECT ... WHERE
  source_path=?1`(SQLite,無網路),沒變的檔案讀完 hash 一比對就跳過,不會觸發 embed
  API。真正貴的是 embed API 那個網路來回,只有**真的變動過的檔案**會觸發——成本只跟
  「這次改了幾個」成正比,不跟「notes/ 總共有幾個」成正比,跟本文件開頭那句「context
  成本恆定」是同一個原理,套用在索引成本上
- reverse 懸空清除那段——`SELECT DISTINCT source_path FROM chunks` 一次查詢 + 跟磁碟
  檔案列表(forward pass 本來就要掃一次,順便拿到,不用多掃一次)做 set 差集,字串集合
  比對,幾千筆也是毫秒等級
- 唯一代價是沒動過的檔案會被重複讀+算 hash 三次(1h/6h/24h 各一次)——雖然便宜,但對
  一個人用的記憶系統(notes 數量現實上是幾十到幾百等級)不算負擔。如果哪天真的長到不現
  實的規模,可以優化成「只 reindex 這一層自己剛剛動過的檔案」,但現在不用先做,過早優化

---

## 6. 成本與 cache

**成本**
- core 讀:~500 token/喚醒 唯讀輸入;開 prompt caching 後近免費
- 維護寫:1h/6h/24h 各一次呼叫,吃的都是下層產物(context 恆定)
- **空轉 short-circuit**:無新素材直接跳過,不產出「這 1 小時沒事發生」的報告——這條
  已經上線驗證過,1h 週期底下空轉幾乎零成本,不需要靠拉長週期來省

**健康指標(做進 gateway usage 面板)**
- **維護 token 佔總量比例**:超過 ~30% 表示系統花在「整理自己」的力氣超過幹活——先看是
  不是 short-circuit 沒生效(該跳過的還在跑 LLM),而非直接放寬週期
- **core.md commit 頻率**:天天改多次 = prompt 太鼓勵動手;幾天一動 = 健康的常識層

**Cache 友善性(順手紅利)**
- core 每日只改一次 → 前綴穩定
- 技能不進 system prompt → 學新技能不動前綴

對比 Hermes 兩個結構性病(ebina 不會得):
| 病 | Hermes 成因 | ebina |
|---|---|---|
| cache miss 稅 | 12-16K 前綴 + 換 model/學 skill 即失效 | core 每日一改;技能不進前綴 |
| skill 索引膨脹 | 索引全進 prompt,隨技能數永久上漲 | hybrid 檢索,context 成本與技能總數無關 |

---

## 7. 容量判準

移到 [002-skill-v2.md](002-skill-v2.md) §4 了(這節純講技能索引的 context 成本,不是
memory 的事)。

---

## 8. 結合兩家優點

跟 Hermes 的整體對照,記憶跟技能都放一起總覽(技能那兩行細節見
[002-skill-v2.md](002-skill-v2.md))。

| 記憶問題 | Hermes | ebina |
|---|---|---|
| 關鍵事實在場 | 小檔全量注入 | **採 Hermes**:core.md 無條件注入 |
| 深記憶成長 | 2.2K 天花板 | **採自己**:notes/ 無上限 |
| 深記憶取用 | FTS 關鍵字 | **採自己**:hybrid(BM25+向量+RRF)|
| 記憶演化 | 反應式寫 skill | **採自己**:三層瀑布蒸餾 + 晉升塔 |
| 技能發現 | 索引全進 prompt | **採自己**:hybrid 檢索浮現 |
| token 成本 | 12-16K 固定稅 | **採自己**:~500 常駐 + 按需 |

---

## 9. TODO

### 瀑布(1h 沿用現行 daily_maintenance 週期,不改)
- [ ] 1h 快維護:吃 since_ts delta(含 `/workspace/memory/` 掃描)→ 直接寫入/更新 notes/ + hourly report;無新素材 short-circuit(已上線,沿用)
- [ ] 6h 中維護:吃近 6 份 1h report(主要輸入,不做 notes/ 全檔掃描)→ 整併(合併重複/解衝突/過時降級,不是第一次寫入)+ 待辦升級(§4,已上線,沿用)+ 技能品質閘門(見 002 §3);動完 notes/ 收尾前呼叫一次 `reindex_all_notes`(見 §5)
- [ ] 24h core 蒸餾(新增第三層):吃近 4 份 6h summary → core 晉升/降級;同樣動完 notes/ 收尾前呼叫一次 `reindex_all_notes`
- [ ] 各層 summary 帶來源指標(wake_id / note 路徑),支援回查原文
- [ ] **修上下層 race**(見上方附註,`gateway.rs` 現有 code 已受影響,不只 24h 新層):
      三層 fire 分鐘錯開(1h `:13`、6h `:18`、24h `:23`),不用共享 flag,靠既有
      checkpoint 自癒兜底極端情況
- [ ] `memory/maintenance_reports/` 搬到頂層 `/maintenance_reports/`(見 §1 路徑附註,
      `memory/` 之後只放 notes/ 跟 index.db)——遷移既有已上線的檔案 + 改
      `gateway.rs` 裡硬寫的路徑常數;加第三層資料夾(`core/`)+ 新 checkpoint
      `.last_core_run`;webui ReportsPanel 加第三個 tab(hourly/summary/core,summary 當 default)
- [ ] `memory/notes/<date>/log.md` 搬到 `/logs/`(見 §1 路徑附註)——遷移既有已上線的
      run log 檔案,改 `agent_loop.rs` 的 `NOTES_DIR`-based 路徑組裝(`{NOTES_DIR}/{date}/log.md`
      兩處)、`gateway.rs` `has_new_log_entries` 的路徑;`reindex_all_notes` 的 `is_dir`
      跳過邏輯可以拿掉(搬完 `memory/notes/` 變純平面目錄,不會再有子目錄)

### 記憶
- [ ] `core.md` + 2K 上限檢查(超限拒寫,同一 run 內讓 LLM 重新取捨最多 2-3 次,見 §2.3)——
      trigger_note 要明講重試耗盡時 `done` 的 `summary` 得以 `"run aborted: "` 開頭,
      不然 `outcome_aborted` 抓不到,現有失敗重試那套永遠不會被觸發
- [ ] 每次喚醒無條件注入 core;core 排除於向量索引
- [ ] 明確跟 `/SOUL.md` 分工:core.md 只放蒸餾事實,不放身份/語氣(見 §2.1)——寫 prompt
      時兩份檔案的注入說明要分開講清楚,避免 agent 自己搞混兩者用途
- [ ] gateway:core 單獨顯示 + git 演化史
- [ ] **修現有的索引懸空洞**(見 §5 附註,不是新問題):`reindex_all_notes` 加反向清除
      ——DB 裡有、磁碟上已經不存在的 `source_path`,整批從 `chunks`/`chunks_fts` 刪掉;
      合併/`delete_path` 掉的 `memory/notes/*.md` 現在完全不會清索引

### 待辦升級
- [ ] 現行機制(見 §4)已上線驗證過,v2 沿用,不用新增改動

### 技能
拆到 [002-skill-v2.md](002-skill-v2.md) §5 了。

### 觀測
- [ ] usage 面板:維護 token 佔比、core commit 頻率

### 驗證(兩週餵養測試,只有 log 能回答)
- [ ] 晉升品質:`git log core.md` 的演化是否越來越像「你的常識」而非抖動白板
- [ ] core 是否真的減少關鍵事實檢索 miss
- [ ] 維護 token 佔比是否合理(<30%)
- [ ] 技能檢索命中率驗證見 002 自己的驗證段