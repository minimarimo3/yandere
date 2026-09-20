# Yandere Companion v0.1.10



## v0.1.11

- macOS の non-activating NSPanel では `window.confirm()` が見えない/応答待ちになる場合があるため、終了確認を「5秒以内にボタンをもう一度押す」方式へ変更。`cargo run` でも `.app` でも同じ終了経路を使います。
- Cargo/Tauri の表示バージョンを 0.1.11 に統一。

## v0.1.10

- 観察エンジンをGroq/QwenからGoogle AIへ移行。`gemma-4-26b-a4b-it` → `gemma-4-31b-it` → `gemini-3.1-flash-lite` の順に自動fallbackします。
- 観察も会話・日記と同じGemini API Keyを利用するため、Groq API Key設定を削除しました。古い `groq_api_key` / `groq_model` は次回設定読込時に `config.toml` から自動除去します。
- Gemmaの観察入力は圧縮せず、従来どおりscreenpipeの直近情報を渡します。
- Gemmaの出力JSONが壊れた場合、HTTP 429/5xx、接続失敗なども次モデルへのfallback条件になります。
- 観察間隔は30〜180秒程度を推奨（設定値自体は従来どおり保持）。

## v0.1.8

- Groq/Qwen observation requests now set `max_completion_tokens: 300` so the on-demand 1000 OTPM limit is not exceeded by a single request.
- A non-retryable Groq 429 caused by the request itself exceeding OTPM is logged once instead of retrying the identical payload.


## v0.1.7

- 設定画面に **「美月とscreenpipeを終了」** を追加。設定したローカルscreenpipeポートのリスナへSIGTERMを送り、必要な場合だけSIGKILLへフォールバックしてからアプリを終了します。
- macOSの表示ウィンドウを通常の `NSWindow` から実際の **`NSPanel`** へ変換。`CanJoinAllApplications` / `CanJoinAllSpaces` / `FullScreenAuxiliary` と ScreenSaver window level を使い、他アプリのフルスクリーンSpace上でも表示できる構成へ変更しました。
- メニューバークリック時のトグルも修正。別Spaceで `visible=true` になっているだけのウィンドウを誤ってhideせず、現在のSpaceでフォーカスされている時だけ閉じます。
- 睡眠中UIから起床予定時刻の表示を削除。「すやすや寝ています。」だけ表示します。起床時刻そのものは内部スケジューラのためだけに保持します。
- 設定画面にアプリバージョン、アプリログ、screenpipeログの保存先を表示。
- 永続ログ `companion.log` を追加。Groq/Gemini/screenpipe APIのHTTP 429/503、再試行、最終エラー、screenpipe起動/終了などを記録します。APIキーやプロンプト本文は記録しません。
- Groqの観察APIも429/5xx時に最大3回リトライするよう変更。
- Git公開用の `.gitignore` を追加。

### ログ保存先

通常のmacOS環境では、設定画面にも実際のフルパスが表示されます。標準では次の場所です。

```text
~/Library/Application Support/local.yandere.companion/companion.log
~/Library/Application Support/local.yandere.companion/screenpipe.log
```

`companion.log` はYandere Companion側のAPI・スケジューラ・エラー記録、`screenpipe.log` はYandere Companionが起動したscreenpipeプロセスのstdout/stderrです。

macOS のメニューバーに常駐して、screenpipe の直近アクティビティを観察し、ときどき自分から話しかける小さな人格エージェントです。

## 今できること

- macOS メニューバーに `♡` として常駐
- クリックすると小さな会話ウィンドウを表示
- screenpipe (`http://127.0.0.1:3030`) の直近アクティビティを定期観察
- Gemma 4 26B（失敗時 31B → Gemini 3.1 Flash Lite）が「作業中か・集中度・何をしているか・今話しかけるか」をJSONで判断
- 話しかける場合、Gemini 3.5 Flash-Lite が自然な短文を生成し macOS ネイティブ通知
- メニューバーから普通に会話（Gemini 3.5 Flash-Lite）
- 毎日22:30ごろ、その日の観察を Gemini 3.8 Flash が「彼女視点の日記」にする
- 毎晩23:00〜23:59のランダムな時刻に寝落ちし、7時間45分〜8時間15分ほど睡眠
- 睡眠中は観察・会話・日記生成を停止し、Google AI/screenpipe検索APIを呼ばない
- packaged `.app` を一度起動すると、macOSログイン時の自動起動を登録
- 起きている間、screenpipe が停止していれば推奨オプション付きで自動起動
- SQLite に観察要約、会話、日記を保存

## 睡眠リズム

その日の睡眠時刻はアプリデータ内の `rhythm.json` に保存されます。アプリを再起動しても、その夜の寝落ち時刻が毎回変わることはありません。

- 日記: 22:30以降、1日1回。スケジューラは15秒間隔なので通常は22:30から十数秒以内。
- 寝落ち: 23:00〜23:59から毎日ランダム
- 睡眠時間: 7時間45分〜8時間15分から毎日ランダム
- 睡眠中: 定期観察なし、手動観察不可、チャット入力不可、Google AI呼び出しなし

screenpipe自体がすでに別プロセスとして動いている場合、それを強制終了はしません。睡眠中に止まるのは **Yandere Companion側の観察・検索** です。起床後は再び直近の時間窓から観察を始めるため、睡眠中の画面をまとめて読み返すことはありません。

## screenpipe 自動起動

起きている状態で観察が始まる前に `/health` を確認し、screenpipe がいなければ次の設定で自動起動します。

```bash
screenpipe record \
  --disable-audio \
  --app-context memory \
  --disable-keyboard-capture=false \
  --disable-clipboard-capture=true \
  --capture-scroll=true \
  --prioritize-input-latency \
  --disable-meeting-detector \
  --disable-telemetry \
  --retention-days 14 \
  --idle-capture-interval-ms 30000 \
  --retention-mode media
```

`screenpipe` コマンドがPATHにあればそれを優先し、なければ login zsh 経由で次を使います。

```bash
npx -y screenpipe@latest record ...
```

GUIアプリをログイン時に起動した場合でも Homebrew / nvm の PATH を拾いやすいよう `/bin/zsh -lc` 経由です。自動起動したscreenpipeの標準出力・標準エラーはアプリデータ内の `screenpipe.log` に追記されます。

すでに自分で `npx -y screenpipe@latest record` を実行している場合は `/health` が通るので、重複起動しません。

## プライバシー方針

- screenpipe の OCR / Accessibility テキストは、Gemma/Gemini が「今何をしているか」を判断するため直近分だけ一時的に送信します。
- OCR / Accessibility の生テキストはこのアプリのDBには保存しません。
- `content_type=input` は回数・イベント種別だけを見る設計で、キー内容は読み取り・API送信しません。
- `--disable-keyboard-capture=false` によりscreenpipe側のローカルDBには入力イベントが保存されますが、Yandere Companionはその本文フィールドを参照しません。
- 保存する観察ログは主にアプリ、ウィンドウ名、入力イベント数、作業判定、集中度、要約です。

## 必要なもの

1. macOS
2. Rust 1.77.2 以上（新しい stable 推奨）
3. Xcode Command Line Tools
4. Node/npm (`npx` fallbackを使う場合)
5. Gemini API key
6. screenpipe Local API Key

screenpipe Local API Key は次で確認できます。

```bash
npx -y screenpipe@latest auth token
```

返ってきた `sp-...` を **設定 → screenpipe Local API Key** に貼ってください。

## 開発実行

```bash
xcode-select --install   # 未導入なら
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

cd src-tauri
cargo run
```

デフォルトモデル:

- 観察・発話判定: `gemma-4-26b-a4b-it` → `gemma-4-31b-it` → `gemini-3.1-flash-lite`
- 会話・通知文: `gemini-3.5-flash-lite`
- 日記: `gemini-3.8-flash`

## .app / DMG とログイン時自動起動

```bash
cargo install tauri-cli --version '^2'
cd src-tauri
cargo tauri build
```

生成された `.app` を `/Applications` などに置いて **一度通常起動**してください。その起動時に Tauri の macOS LaunchAgent 方式でログイン時自動起動を登録します。

開発中の `cargo run` では、`target/debug/...` をLaunchAgentへ登録してしまわないよう自動起動登録を意図的に行いません。

## キャラクター調整

メニューバー → 設定 → **人格** を直接編集できます。

基本方針は「普通の恋人が基調で、独占欲や嫉妬は必要な場面だけ薄く滲む」です。PC観察は周辺視野として扱い、挨拶や普通の雑談で毎回監視事実を持ち出さないようにしています。

## 通知頻度

観察モデルが `should_speak=true` を返しても、アプリ側で次を強制します。

- 通知間隔: デフォルト20分以上
- 1時間: 最大2回

## screenpipe のWARNについて

次のログは単発なら致命的ではありません。

- `frame_linker: stale entries expired without pairing`: 入力イベントと対応フレームの紐付けが一件タイムアウト。大量に増え続けなければ観察全体は継続します。
- `AXLineForIndex failed ... paragraph bbox`: 行単位座標が取れず段落矩形へフォールバック。テキスト取得そのものの失敗ではありません。
- `search request rejected: route-wide admission is full`: screenpipeの検索同時実行制限。アプリ側で `Retry-After` に従い最大4回リトライします。

## v0.1.5

- 日記の自動生成時刻を22:30へ変更。
- 23:00〜24:00に毎日ランダムで寝落ちし、約8時間眠る生活リズムを追加。
- 睡眠計画を `rhythm.json` へ永続化し、再起動でも同じ夜の予定を維持。
- 睡眠中は観察・会話・日記生成と外部LLM API呼び出しを停止。
- Tauri official autostart pluginを追加し、packaged `.app` の初回起動時にmacOSログイン自動起動を登録。
- screenpipeが停止していれば推奨オプション付きで自動起動。手動起動済みなら重複起動しない。
- UI外枠のスクロールを廃止。チャット履歴・設定・日記本文の内部スクロールだけを残した。
