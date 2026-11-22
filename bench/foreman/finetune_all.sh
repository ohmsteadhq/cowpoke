#!/usr/bin/env bash
# Fine-tune several small models on the same generated data, then benchmark each.
# Unattended: a failing model is logged and skipped. Status lines go to $LOG.
#
#   bash bench/foreman/finetune_all.sh <cowpoke password> [model ...]
set -u
cd "$(dirname "$0")"
PASS=${1:?cowpoke admin password}; shift
MODELS=("$@")
[ ${#MODELS[@]} -eq 0 ] && MODELS=(
  "gemma4-e2b|mlx-community/gemma-4-e2b-it-4bit"
  "smollm3-3b|mlx-community/SmolLM3-3B-4bit"
  "llama3.2-3b|mlx-community/Llama-3.2-3B-Instruct-4bit"
  "qwen3-1.7b|mlx-community/Qwen3-1.7B-4bit"
)
PY=~/.venvs/mlx/bin
RUNS=data/mlx/runs
LOG=$RUNS/status.log
mkdir -p "$RUNS"
say() { echo "$(date '+%H:%M') $*" | tee -a "$LOG"; }

# Wait for any benchmark run already going (match the Python process, not shells).
while pgrep -f "Python.*bench.py --model" >/dev/null; do sleep 30; done

for entry in "${MODELS[@]}"; do
  name=${entry%%|*}; hf=${entry#*|}
  dir=$RUNS/$name
  mkdir -p "$dir"
  say "== $name ($hf): training"
  cat > "$dir/lora.yaml" <<EOF
model: $hf
train: true
data: data/mlx
fine_tune_type: lora
mask_prompt: true
num_layers: 8
batch_size: 1
iters: 500
learning_rate: 1.0e-4
steps_per_report: 50
steps_per_eval: 500
val_batches: 8
save_every: 250
max_seq_length: 1024
grad_checkpoint: true
adapter_path: $dir/adapters
lora_parameters: {rank: 8, scale: 20.0, dropout: 0.0}
seed: 1
EOF
  if ! $PY/mlx_lm.lora --config "$dir/lora.yaml" > "$dir/train.log" 2>&1; then
    say "$name: training FAILED: $(tail -1 "$dir/train.log" | cut -c1-200)"; continue
  fi
  say "$name: trained; val $(tr '\r' '\n' < "$dir/train.log" | sed 's/\x1b\[[0-9;]*m//g' | grep -E '^ *500 +val' | awk '{print $3}')"
  if ! $PY/mlx_lm.fuse --model "$hf" --adapter-path "$dir/adapters" --save-path "$dir/fused" --dequantize > "$dir/fuse.log" 2>&1; then
    say "$name: fuse FAILED: $(tail -1 "$dir/fuse.log" | cut -c1-200)"; continue
  fi
  printf 'FROM ./fused\nPARAMETER temperature 0\n' > "$dir/Modelfile"
  if ! (cd "$dir" && ollama create "cowpoke-foreman:$name" -f Modelfile --quantize int4 > create.log 2>&1); then
    say "$name: ollama import FAILED: $(tail -1 "$dir/create.log" | tr -d '\r' | cut -c1-200)"; continue
  fi
  rm -rf "$dir/fused"
  for r in "--rules" ""; do
    out=$(python3 bench.py --model "cowpoke-foreman:$name" --mode evidence $r --no-format --think default --password "$PASS" 2>&1 | tail -1)
    say "$name ${r:-(no rules)}: $(echo "$out" | python3 -c 'import sys,json
try:
  s=json.loads(sys.stdin.read()); print(f"passed {s[\"passed\"]}/12, service {s[\"service_ok\"]}, cause {s[\"cause_ok\"]}, {s[\"avg_seconds\"]} s")
except Exception as e: print("bench FAILED")')"
  done
done
say "ALL DONE"
