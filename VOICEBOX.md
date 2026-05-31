## VOICEVOX 処理アーキテクチャ

Nelfie の VOICEVOX は外部の VOICEVOX Engine サーバーを使わず、`voicevox_core` をプロセス内で直接ロードするスタンドアローン構成です。

起動時は `NelfieContext::new` で `VoiceSystem` を作り、`VOICEVOX_PRELOAD_ON_STARTUP=true` の場合は Discord bot の起動前に `VoiceSystem::initialize_on_startup` で core を初期化します。初期化では `CoreRuntime` が ONNX Runtime、OpenJTalk 辞書、VVM モデルを準備し、`Synthesizer` を構築します。必要なアセットがローカルに無い場合は、対応する ONNX Runtime、OpenJTalk 辞書、VOICEVOX VVM を自動で取得します。

音声設定は用途ごとに分かれています。ギルド単位の接続状態、既定話者、最後のエラーは `VoiceSystem` が持ち、テキストチャンネル単位の自動読み上げ、読み上げ辞書、並列読み上げ数は `ChatContexts` が持ちます。ユーザーごとの話者、速度、音程、左右パン、ユーザー辞書は `UserContexts` に保存されます。

読み上げ要求は slash command、LLM の `voicevox-tool`、または自動読み上げから `VoiceSystem::speak` に集約されます。ここで本文の正規化、長文時の最低速度補正、話者とチャンネル設定の解決を行い、テキストチャンネル単位の mpsc キューへ投入します。キューは順序制御用で、同一チャンネル内の読み上げを詰め込みすぎないようにします。

実際の処理は `process_speak_request` が担当します。本文は `split_tts_segments` で短いセグメントに分割され、各セグメントを `CoreRuntime::synthesize` で WAV に変換します。再生待ちを短くするため、現在のセグメントを再生している間に次のセグメントを 1 つだけ先読み合成します。`vc_config` の並列読み上げ数が 2 以上の場合は、チャンネルごとの `Semaphore` で同時実行数を制御します。

合成後の WAV は必要に応じて左右パンを適用し、ファイルには書き出さずメモリ上のまま Songbird に渡します。通常は `enqueue_input` で Discord VC の再生キューに積み、並列読み上げ時は `play_input` と再生終了待ちでパイプラインを進めます。これにより、VOICEVOX の合成、Bot 内キュー、Discord VC 再生がそれぞれ独立しつつ、読み上げ順序と負荷を制御できる構成になっています。

## めも
だいたいSynthesizer内でONNXよぶときONNX自体がスレッドランタイムもつからSynthをArcでもって共有するならONNXへの割り当てスレッド数考慮しないとだめだよな  
根本的に並列での処理をうまいことできるのかはOS任せ  
GPUアクセラレーション使う場合結局意味ない  