<div align="center">
<h1>Nelfie</h1>
</div>

Nelfie(ネルフィー) は、Discord 上で会話・ツール実行・VOICEVOX 読み上げを統合して動かす Rust 製の bot です。

Observer-rust の後継プロジェクトとして、スタンドアローン動作を前提に設計されており、OpenAI API を利用した会話機能を中心に、様々なツールや機能を提供します。

## 機能
- 会話: OpenAI API を利用した会話機能 モデル選択可能 レート制限可能 システムプロンプト設定可能
  - tools:
    - get_time.rs: 国コードから現在の時刻を取得
    - latex.rs: TeX 数式を画像化
    - modal_builder.rs: モーダルダイアログを動的に構築
    - discord.rs: Discord 操作系ツール（例: メッセージ送信、チャンネル管理）
    - voicevox.rs: VOICEVOX 操作系ツール（例: 話者変更、スタイル変更、読み上げ）
- VOICEVOX 読み上げ: VC内でのテキスト読み上げ機能 話者・スタイル選択可能 自動読み上げ機能
- TeX 数式の画像化: TeX 数式を画像化して Discord に送信する機能

## システム要件
- OS: Windows 10/11 (64-bit), Linux (x86_64)
- RAM: 3GB以上（ページアウトを期待できるなら2GBでも動作可能）
- CPU: x86_64-v3(AVX2をサポートしていなければなりません)
- ストレージ: 4GB以上の空き容量（VOICEVOXモデルとONNX Runtimeを含む）

ONNX Runtime の HWアクセラレーション については[voicevox_onnxruntime](https://github.com/VOICEVOX/onnxruntime-builder/releases)のリリースノートを参照してください。

## セットアップ

1. リリースからバイナリをダウンロード

2. `.env` を作成

```dotenv
# required
DISCORD_TOKEN=xxxxxxxxxxxxxxxx
OPENAI_API_KEY=sk-xxxxxxxxxxxxxxxx

# 以下optional
SYSTEM_PROMPT=あなたのシステムプロンプト
VOICEVOX_DEFAULT_SPEAKER=3
VOICEVOX_CORE_ACCELERATION=auto
VOICEVOX_CORE_CPU_THREADS=0
VOICEVOX_CORE_LOAD_ALL_MODELS=false
VOICEVOX_OUTPUT_SAMPLING_RATE=48000 # 24000 の倍数であるべきです
VOICEVOX_PRELOAD_ON_STARTUP=true
VOICEVOX_OPEN_JTALK_DICT_DIR=voicevox_core/dict/open_jtalk_dic_utf_8-1.11
VOICEVOX_VVM_DIR=voicevox_core/models/vvms
# 実行時リンク(load-onnxruntime)では voicevox_onnxruntime を自動探索してロードします
# 通常は未設定でOK。明示パス/ファイル名を指定したい場合のみ設定してください
VOICEVOX_ONNXRUNTIME_FILENAME=
```

3. 起動

v0.1.1 以前はVOICEVOXの初期化がlazyなため使用時に数分間の待ち時間があります。  
初期化終了後は次回起動以降も高速に起動します。　　
v0.1.4以降はserenityの起動とdownloaderの起動が直列化しました。


## 設定

現在の実装では、環境変数（`.env`）から読み込みます。
`DISCORD_TOKEN` と `OPENAI_API_KEY` は未設定だと起動時にエラーになります。

## 主なコマンド

この bot は slash command と prefix command（`!`）の両方を登録します。

基本:

- `/ping`: Discord API との遅延を測定して返す
- `/enable`: ChatBot 機能を有効化
- `/disable`: ChatBot 機能を無効化
- `/clear`: 会話履歴をクリア
- `/model`: 使用する OpenAI モデルを選択
- `/rate_config`: レート制限の設定(管理者のみ)
- `/set_system_prompt`: システムプロンプトの設定(管理者のみ)
- `/tex_expr`: TeX 数式を画像化して送信

VC / TTS:

- `/vc_join [auto_read]`: ボイスチャンネルに参加します。オプションで自動読み上げを有効化できます。
- `/vc_leave`: ボイスチャンネルから退出します。
- `/vc_say <text>`: 指定したテキストを読み上げます。
- `/vc_autoread <enabled>`: 自動読み上げの有効/無効を切り替えます。
- `/vc_dict <source> <target>`: 読み上げの辞書エントリを追加/削除します。
- `/vc_speaker ...`: 話者, スタイル, 音程, 速さ, パンの設定を行います。
- `/vc_status`: 現在の VC 状態と VOICEVOX 設定を表示します。
- `/vc_config ...`: 読み上げの詳細設定を行います。(自動読み上げ, システム読み上げ, 並列読み上げ)

## 開発用チェック

```powershell
cargo clippy --all-targets --all-features -- -D warnings
```

fmtはやってない。

## トラブルシュート

- `DISCORD_TOKEN must be set`:
	`.env` に `DISCORD_TOKEN` を設定してください。
- `OPENAI_API_KEY must be set`:
	`.env` に `OPENAI_API_KEY` を設定してください。
- VOICEVOX 関連で辞書/モデル/onnxruntime が見つからない:
	`.env` の VOICEVOX 系パス、または `voicevox_core` 配下のディレクトリ構成を確認してください。

## Third-party licenses

- KaTeX font のライセンスは [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md) に記載しています。
- `KaTeX_font` ディレクトリを再配布する場合は、上記ライセンス表記を同梱してください。

ダウンロードコンテンツはREADMEを含むのでそちらを参照。
