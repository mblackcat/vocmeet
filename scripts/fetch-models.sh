#!/usr/bin/env bash
# 下载并校验 VocMeet 所需的 ONNX 权重。
#
# 用法：  bash scripts/fetch-models.sh [目标目录]
# 默认目标目录为 ./models
#
# 权重不入库（见 .gitignore）。所有资产来自 sherpa-onnx 官方 GitHub Release。
# 许可证注意：SenseVoice / CT-Punc 属阿里 FunASR 模型开源协议，商业化前需法务结论，
# 详见 docs/vocmeet_mvp_plan.md §8.2。

set -euo pipefail

DEST="${1:-models}"
BASE="https://github.com/k2-fsa/sherpa-onnx/releases/download"

mkdir -p "$DEST"
cd "$DEST"

fetch() { # url filename
  local url="$1" name="$2"
  if [ -f "$name" ]; then
    echo "  [skip] $name (已存在)"
    return
  fi
  echo "  [get ] $name"
  curl -SL --fail --retry 3 --retry-delay 2 -o "$name.part" "$url"
  mv "$name.part" "$name"
}

untar() { # archive marker_dir
  local archive="$1" marker="$2"
  if [ -d "$marker" ]; then
    echo "  [skip] $marker (已解压)"
    return
  fi
  echo "  [tar ] $archive"
  tar xf "$archive"
}

echo "==> ASR: SenseVoice int8 (2025-09-09)"
fetch "$BASE/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09.tar.bz2" \
      "sense-voice.tar.bz2"
untar "sense-voice.tar.bz2" "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09"

echo "==> VAD: Silero"
fetch "$BASE/asr-models/silero_vad.onnx" "silero_vad.onnx"

echo "==> Diarization: pyannote segmentation 3.0 (MIT)"
fetch "$BASE/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2" \
      "pyannote-seg.tar.bz2"
untar "pyannote-seg.tar.bz2" "sherpa-onnx-pyannote-segmentation-3-0"

echo "==> Speaker embedding: 3D-Speaker eres2net (Apache-2.0)"
fetch "$BASE/speaker-recongition-models/3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx" \
      "speaker-embedding.onnx"

echo "==> Punctuation: CT-Transformer zh-en int8"
fetch "$BASE/punctuation-models/sherpa-onnx-punct-ct-transformer-zh-en-vocab272727-2024-04-12-int8.tar.bz2" \
      "punct.tar.bz2"
untar "punct.tar.bz2" "sherpa-onnx-punct-ct-transformer-zh-en-vocab272727-2024-04-12-int8"

echo "==> 测试音频（四人中文对话，用于冒烟测试）"
fetch "$BASE/speaker-segmentation-models/0-four-speakers-zh.wav" "test-four-speakers-zh.wav"

echo
echo "完成。运行 \`vocmeet-cli doctor --models $DEST\` 校验完整性。"
