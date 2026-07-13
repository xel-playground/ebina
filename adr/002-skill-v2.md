# ebina — 技能(程序記憶)子系統設計 v2

從 [001-memory-v2.md](001-memory-v2.md) 拆出來的技能專章——原本混在同一份文件裡,技能
跟記憶(core/notes)是兩個不同的子系統,分開比較不會混在一起討論。

**跟 001 的關係**:技能索引(`skill_chunks`)是 001 §1 全景瀑布圖裡「檢索層」的一部分
(`index.db 檢索層 = notes chunks + skill_chunks,獨立 table,不混用`);技能的品質閘門
(§3 這份文件)掛在 001 的 6h 中維護層執行(不是 24h core 蒸餾——兩者互不依賴,6h 頻率
也讓沒用的技能不會晾一整天才被複查)。除此之外技能子系統自成一格,不吃 1h/6h report
(品質閘門雖然掛在 6h 這次 wake,但不讀 1h/6h report 內容,只看 skill 檔案自己的
`retrieval_hit_count`)——這份文件單獨講。

---

## 1. 儲存

```
/skills/<name>.md    # frontmatter 描述段(何時用/做什麼,50-80 token)+ 正文
```
**從 `memory/skills/` 搬出來,獨立成頂層目錄**(跟 `/workspace/`、`/logs/` 同一層)——見
001 §1 路徑附註:`memory/` 之後只放 `notes/` 跟 `index.db`,skills 是完全獨立子系統(不吃
1h/6h report,不進記憶蒸餾那套權威/新鮮度規則),放在 `memory/` 底下容易誤導。單檔案格式
本身不變(不改成目錄),舊路徑(`memory/skills/*.md`)是既有已上線的檔案,要遷移。**沒有
`run.*` 腳本**——`exec_wasm` 的沙箱隔離保證已經回退(見 PROJECT.md §7 安全檢查清單),
現在沒有能安全跑 skill 腳本的機制,這版不假設它存在;純文字程序記憶(agent 自己照著
SKILL.md 的步驟用既有 syscall 執行),不是可執行檔。

---

## 2. 檢索式發現(hybrid search,不進 system prompt)

- **只 embed frontmatter 描述段,不 embed 正文**:進 `skill_chunks` 的是 SKILL.md 開頭
  那段 50-80 token 的描述(何時用/做什麼),不是整份檔案。這點要在實作時特別注意——如果
  直接把整份檔案文字丟進 `memory.rs` 現有的通用 chunker(`reindex_file`/`write_reindexed_rows`,
  原本設計給長篇 notes 切段用),正文步驟會被切成好幾個 chunk 混進索引,檢索撈到的會是
  程序片段而不是「這技能是幹嘛的」,違背兩段式設計的本意(發現只看描述,載入才看正文)。
  但**現有 `reindex_file(source_path, embed_model)` 簽名不支援這件事**——內部自己
  `read_to_string(source_path)` 讀整份檔案,沒有接受外部文字的介面(查過
  `memory.rs:146-147` 確認)。要做到只 embed 描述段,`reindex_file` 要拆成兩層:內部新增
  `reindex_text(source_path, text, embed_model)` 做原本 hash/chunk/embed/寫入那些邏輯,
  吃文字不吃路徑;`reindex_file` 改成薄包裝(自己 `read_to_string` 後呼叫
  `reindex_text`)——notes 現有呼叫方(`reindex_all_notes`)完全不用改。skill 專用的索引
  路徑直接呼叫 `reindex_text(path, 抓出來的描述段文字, embed_model)`,跳過整檔讀取
- **獨立 `skill_chunks` table**(+ 自己的 `skill_chunks_fts`),跟 `chunks`/`chunks_fts`
  分開,不用 type 欄位混用同一張表——查詢邏輯不用每次多一個 filter,配額分離也更直覺
- **刪除要連 vector 一起清**:`DELETE /api/skills/{name}`(現有 endpoint)現在只刪
  `/skills/<name>.md`,以後 skill 一旦進了 `skill_chunks`,刪除路徑要一併
  `DELETE FROM skill_chunks WHERE source_path = ...`(連 `skill_chunks_fts` 一起)——
  不然刪掉的技能描述還留在索引裡,檢索撈得到但 `use_skill`/`read_file` 對應的檔案已經
  不存在,變成懸空結果
- **`save_skill` 當場 embed,不等維護週期**:現有 `save_skill`(`agent_loop.rs:466`)只
  `skills::save()` 寫檔案,沒有順手進索引這一步——如果比照 notes 現在的行為(`write_file`
  當下不 embed,靠 1h daily_maintenance 自己的「content hash 增量」步驟才重新掃描),剛
  存的新技能會有最長接近 1h 的空窗完全搜不到。技能存檔本來就是單筆、低頻的動作(不像
  notes 一次 run 可能改好幾個檔),`save_skill` 存檔當場呼叫 `reindex_text`(§2 上面那條
  拆出來的文字版,傳描述段而不是路徑)把這一筆寫進 `skill_chunks`,存好立刻就能被檢索到,
  不用等下一輪維護
- **1h 層也要加 `reindex_all_skills`,不能只靠 `save_skill` 當場那次**:「embed_model 不符
  → 自動全庫重嵌」這個保證,機制上就是 `reindex_all_notes` 每次 1h 把 `memory/notes/`
  全部重掃一遍——現在只掃 notes,不掃 skills。如果 skill_chunks 只靠 `save_skill` 存檔
  當下那一次性 embed,以後真的換 embed model,notes 會自動全部重嵌,但 skill_chunks 永遠
  停在舊 model 的向量,沒人補,查詢語意直接失準且沒有任何錯誤訊息。兩者分工不衝突:
  `save_skill` 當場 embed 管**新鮮度**(存了立刻能搜到),1h 週期性 `reindex_all_skills`
  管**一致性**(embed_model 換掉、或有人繞過 `save_skill` 直接改檔案時的保底),同一次
  1h pass 跟 `reindex_all_notes` 一起跑,同一套 content-hash 增量判斷邏輯
- **`reindex_all_skills` 也要有反向懸空清除,不是只有正向增量**:001 §5 幫 notes 修的
  「反向比對 DB `source_path` 跟磁碟現存檔案,清掉懸空列」,只寫給 `reindex_all_notes`——
  `reindex_all_skills` 目前的描述只有正向(掃現存檔案、增量重嵌),沒有反向。如果有人
  直接 `delete_path` 刪掉一個 skill 檔案(繞過 `DELETE /api/skills/{name}` 那個有明確
  清索引步驟的 endpoint),`skill_chunks` 會永遠留著懸空列,跟 notes 同一個洞。
  `reindex_all_skills` 要比照 `reindex_all_notes` 修完後的版本,同樣做反向比對+清除,
  不是只抄正向那半
- **`embed_model` 是全專案單一設定,不是每個 table 各自的**:讀的是 `/config.toml`
  `[embed]` 那個全域值(`memory.rs` `current_embed_model()`,查過確認),`chunks` 跟
  `skill_chunks` 各自的 `embed_model` 欄位只是逐 row 記錄「這筆是用哪個 model 嵌的」,
  寫進去的值永遠來自同一次呼叫拿到的同一個字串——沒有 notes 用一個 model、skills 用另一
  個這種分裂設計。換 model 時兩張表要在同一次 1h pass 一起重嵌(上面那條已經這樣寫),
  避免兩張表暫時性不一致
- **配額分開**:記憶 top-k,技能另取 top 2-3,各自查完在 RRF 那層合併,互不搶池
- **兩段式**:發現靠檢索(注入描述行)→ 載入靠主動(要用才 read_file 完整 SKILL.md)
- **存在提示**:system prompt 一行「你在 skills/ 有技能庫,動手前可查」(~20 token,不隨技能數膨脹)——解「想不到要查」
- **主動查找**:新增 `skill_search` action,對應現有 `memory_search`(查 notes/)的技能版
  ——自動檢索(每次喚醒用 trigger 文字當 query)沒撈到,但 agent 自己覺得可能有相關技能時,
  主動查一次,不用等下一次自動檢索
- **自動檢索頻率**:notes 現在是「每個 tool result 都重新查一次」(話題跑掉會跟著換);
  skill_chunks 建議只在 run 一開始查一次就好,用最初的 trigger 文字當 query——技能通常
  對應整個任務性質,不太會像記憶那樣隨對話中途跳題,每個 tool result 都重查沒必要,白花
  一次 embed + 檢索的成本
- **不做「高頻晉升進 core」**(原本這裡有這個想法,拿掉了):天天用的技能本來就會一直被
  hybrid 檢索撈到,不需要額外機制把它塞進 core——001 §2.2 的 core 內容只定義兩類(當前
  重心、agent 職責),技能描述硬塞進去也對不上任何一類,不如不做。高頻只是「常常被檢索
  命中」,不代表它需要變成無條件常駐的東西

兩層覆蓋:知道有庫(提示)+ 相關技能自動浮現(檢索)+ 想到才查(`skill_search`)。

**追蹤 `retrieval_hit_count`,不是只有 `used_count`**:現有 `used_count`/`last_used`
(`agent/src/skills.rs`)記的是 `use_skill` 真的被載入過幾次,不是「被檢索撈到過幾次」
——兩個不同的數字,一個技能可以常常被撈到但從沒被用(描述寫得不夠準,agent 看了不採用),
也可能反過來。§3 品質閘門判「從未被檢索命中」需要的是後者,現有系統沒有這個追蹤。加法
比照 `used_count` 現成的做法(存在 SKILL.md 自己的 frontmatter,同一套 read-modify-write):
每次 hybrid search 命中一個 skill_chunks row 就把對應技能的 `retrieval_hit_count` +1——
自動檢索(run 開頭的 hybrid search)命中要算,`skill_search`(主動查,見上方)命中也要算,
兩種都是「被檢索撈到」,§3 判準看的是有沒有被找到過,不分自動/主動觸發。

---

## 3. 品質閘門(掛在 001 的 6h 中維護層,不是 24h core 蒸餾——兩者互不依賴,見開頭
「跟 001 的關係」)

- 「從未被檢索命中」的技能(`retrieval_hit_count` 讀 SKILL.md frontmatter,見 §2)→
  重寫描述或淘汰(防自我增肥)
- 合併語義相近的技能
- **索引更新不用特例處理**:001 §1「誰改 notes/ 誰就自己 reindex」這條原則已經是
  每次 run 收尾都會做的事(不分哪個 trigger),品質閘門對 SKILL.md 的寫入(重寫描述/
  刪除)自然跟著這次 run 自己的收尾一起被 reindex,不需要另外交代 prompt 這件事
  ——machine 層面已經處理掉,不用讓 LLM 知道或操心

---

## 4. 容量判準

| 模式 | context 成本 | 天花板 |
|---|---|---|
| 全量索引進 prompt(Hermes) | 40-60 token/技能;>15-20 個開始不划算 | context 塞不下 |
| hybrid search(ebina) | 恆定 top 2-3 ≈ 150-250 token,**與總數無關** | 檢索撈不準(數百個時描述語義相近)|

數十技能內兩邊都碰不到痛點;採 hybrid 無未來重構債。

---

## 5. TODO

- [ ] SKILL.md frontmatter 描述段規範
- [ ] `SKILLS_DIR` 從 `memory/skills` 搬到頂層 `/skills`(`kernel/src/gateway.rs`、
      `agent/src/skills.rs` 兩處常數都要改)+ 遷移既有已上線的 `memory/skills/*.md` 檔案
- [ ] 新增獨立 `skill_chunks`/`skill_chunks_fts` table(仿 `chunks`/`chunks_fts` schema,不混用)
- [ ] `memory.rs` 的 `reindex_file` 拆成 `reindex_text(source_path, text, embed_model)`
      (實際 hash/chunk/embed/寫入邏輯,吃文字)+ 薄包裝 `reindex_file`(讀檔後呼叫
      `reindex_text`,notes 現有呼叫方不用改)——skill 只 embed 描述段需要這個(見 §2)
- [ ] `save_skill`(`agent_loop.rs:466`)當場呼叫 `reindex_text`(描述段文字)進
      `skill_chunks`,不等下一輪 1h 維護才索引
- [ ] 1h 層加 `reindex_all_skills`(仿 `reindex_all_notes`,掃 `/skills/*.md`,每個
      檔案抓描述段呼叫 `reindex_text`),跟 `reindex_all_notes` 同一次 1h pass 一起跑,
      補上 embed_model 換掉時的全庫重嵌保底
- [ ] `reindex_all_skills` 也要有反向懸空清除(比照 001 §5 修完的 `reindex_all_notes`,
      不是只抄正向增量那半)——`delete_path` 繞過 `DELETE /api/skills/{name}` 直接刪檔案
      時,`skill_chunks` 才不會留懸空列
- [ ] `delete_skill`(現有 `DELETE /api/skills/{name}`)一併清掉 `skill_chunks`/
      `skill_chunks_fts` 對應列,不留懸空索引
- [ ] `retrieval_hit_count` 加進 SKILL.md frontmatter(比照現有 `used_count`/`last_used`
      的 read-modify-write),自動 hybrid search 命中 + `skill_search` 主動查命中都要 +1
      ——§3 品質閘門判準要用
- [ ] 檢索配額分離(記憶 top-k / 技能 top 2-3),RRF 合併
- [ ] 兩段式載入;system prompt 存在提示一行
- [ ] 新增 `skill_search` action(比照現有 `memory_search`),自動檢索沒命中時主動查
- [ ] 6h 中維護(001 的中維護層,不是 24h):未命中技能(`retrieval_hit_count` == 0)
      重寫/淘汰、合併相近(見 §3)——索引更新不用特例處理,同一次 run 收尾本來就會
      呼叫 `reindex_all_skills`

### 驗證
- [ ] 技能檢索命中率(`retrieval_hit_count`);未命中者的描述是否需重寫
