# VOICEVOX 処理アーキテクチャ

Nelfie は外部の VOICEVOX Engine を呼び出さず、`voicevox_core` を同じプロセス内で利用します。音声合成と Discord VC 接続・再生は別の責務として `VoiceSystem` 内で連携します。

## 初期化

`main` は Discord bot の開始前に `NelfieContext::initialize_before_bot_start` を呼び、`VoiceSystem::initialize_on_startup` から `CoreRuntime` を初期化します。起動時初期化は常に実行されます。

`VoiceVoxInitDownloader` は ONNX Runtime、OpenJTalk 辞書、VVM の準備を並行して行います。必要な asset が見つからない場合は取得し、辞書・モデルを指定ディレクトリへ配置します。その後 `CoreRuntime` が `Synthesizer` と style ID から VVM への対応を保持します。

VVM の更新では、既存モデルを残したまま新しい VVM と、リリースに含まれていれば README / TERMS を一時ディレクトリへ取得し、VVM の取得完了後に入れ替えます。取得に失敗すれば既存 VVM を使って起動を続けます。入れ替えに失敗した場合も旧 VVM の復元を試み、復元できない場合は回収用ファイルを残して起動エラーにします。初回取得に失敗し、既存 VVM がない場合も起動エラーになります。

現在の `CoreRuntime` は対応するバージョンの VVM をまとめて読み込みます。環境変数の一覧と既定パスはリポジトリ直下の [README.md](../README.md) を参照してください。

## 状態の分担

- `VoiceSystem` は core runtime、Songbird manager、guild ごとの VC 接続と speaker 設定、text channel ごとの音声キューを管理します。
- `ChatContexts` は channel ごとの自動読み上げ設定、system read 設定、並列数、読み上げ辞書を管理して JSON に保存します。
- `UserContexts` はユーザーの speaker、速度、音程、左右 pan、個人読み上げ辞書を管理して JSON に保存します。
- `voice::text` は本文の正規化、global/channel/user 辞書の適用、TTS セグメント分割を行います。

これらの設定は同じ用途に重複保存されるとは限りません。変更時は `src/llm/channel.rs`、`src/llm/user.rs`、`src/voice/system.rs` の所有範囲と永続化を確認してください。

## 合成から再生まで

`/vc say`、LLM の `voicevox-tool`、自動読み上げは `VoiceSystem::speak` に集約されます。ここで読み上げ本文を正規化し、speaker と channel 設定を解決して、text channel ごとの bounded mpsc queue に要求を送ります。queue 容量は 16 件で、満杯の場合は最大 2 秒だけ enqueue を待ちます。`/vc download` は `synthesize_wav` で WAV を生成してファイルとして送信し、Songbird の再生 queue は通しません。

channel worker は semaphore で同時要求数を制御します。設定可能な並列数は 1〜4 件です。各要求は `split_tts_segments` で分割され、現在のセグメントを再生している間に次の 1 セグメントを先行合成します。先読みは 1 セグメントに制限し、メモリ使用量を抑えます。合成は 45 秒、セグメント再生は 30 秒で timeout します。

合成した WAV はファイルへ書かず、メモリ上の buffer から Songbird に渡します。通常モードでは Songbird の再生 queue に追加し、並列モードでは再生終了を待って要求を進めます。必要な場合は PCM WAV に左右 pan を適用します。

## VC 自動切断

他の利用者が VC から退出した際、event handler は短い待機後に bot が同じ channel に接続中であり、利用者がいないことを再確認してから切断します。さらに 15 秒周期の sweep でも接続中の guild を確認します。切断後は対象 channel の自動読み上げを無効にし、既存の通知を送ります。

この二段階確認により、voice state event の一時的な状態だけで切断を決めません。イベント処理と周期確認は `src/discord/voice_events.rs`、キューと再生は `src/voice/system.rs`、VOICEVOX runtime と asset 準備は `src/voice/core_runtime.rs` にあります。
