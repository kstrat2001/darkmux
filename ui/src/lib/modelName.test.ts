import { describe, expect, it } from "vitest";
import { lastPathSegment, modelShortName } from "./modelName";

// (#3554) Real model ids, grepped from this repo's profiles, fixtures, goldens
// and docs (the file each came from is beside it), and the short name the
// fleet card's count line shows for each.
const TABLE: [id: string, short: string, from: string][] = [
  // The operator's own examples.
  ["darkmux:qwen/qwen3.6-35b-a3b-mlx-4bit", "qwen3.6-35b-a3b", "operator, #3554"],
  ["flint-int4", "flint", "CHANGELOG.md"],
  ["gpt-4o", "gpt-4o", "crates/darkmux-crew/src/dispatch_internal_tests.rs"],
  // The namespace and the publisher or path prefix.
  ["darkmux:qwen3.6-35b-a3b", "qwen3.6-35b-a3b", "crates/darkmux-crew/src/dispatch.rs"],
  ["darkmux:mistralai/devstral-small-2-2512", "devstral-small-2-2512", "docs/demo/demo-flow.jsonl"],
  ["darkmux:gpt-oss-120b", "gpt-oss-120b", "tests/e2e/mission-lens-agentic-turns.spec.js"],
  ["openai/gpt-oss-20b", "gpt-oss-20b", "crates/darkmux-profiles/src/gestalt_host/lms_host.rs"],
  ["google/gemma-4-e4b", "gemma-4-e4b", "src/init_tests.rs"],
  // Quantization and format suffixes.
  ["darkmux:qwen3.6-35b-a3b-turboquant-mlx", "qwen3.6-35b-a3b", "crates/darkmux-crew/src/dispatch_internal.rs"],
  ["majentik/Qwen3.6-35B-A3B-TurboQuant-MLX-MXFP4", "Qwen3.6-35B-A3B", "crates/darkmux-profiles/src/gestalt_host/arch_facts.rs"],
  ["darkmux:TensorFold/Qwen3.8-27B-MLX-4bit", "Qwen3.8-27B", "crates/darkmux-profiles/CLAUDE.md"],
  ["mlx-community/Qwen3.6-35B-A3B-4bit", "Qwen3.6-35B-A3B", "crates/darkmux-profiles/src/engine/tensorfold.rs"],
  ["lmstudio-community/Qwen3-Coder-30B-MLX-4bit", "Qwen3-Coder-30B", "crates/darkmux-profiles/src/lms.rs"],
  ["mlx-community/Qwen3-1.7B-MLX-MXFP4", "Qwen3-1.7B", "src/main.rs"],
  ["qwen3-1.7b-mlx-mxfp4-fast", "qwen3-1.7b-fast", "src/main.rs"],
  ["darkmux:qwen3-coder-next-mlx", "qwen3-coder-next", "docs/demo/demo-fleet.json"],
  ["mlx-community/Muse-Glimmer-30B-8bit", "Muse-Glimmer-30B", "src/init.rs"],
  ["mlx-community/phi-4-8bit", "phi-4", "crates/darkmux-profiles/src/model_ledger.rs"],
  ["flint-int8", "flint", "crates/darkmux-crew/src/engine_dispatch_tests.rs"],
  ["llama-70b-gguf", "llama-70b", "crates/darkmux-profiles/src/model_ledger.rs"],
  ["lmstudio-community/phi-4-gguf", "phi-4", "crates/darkmux-profiles/src/gestalt_host/gguf_facts.rs"],
  ["microsoft/phi-4-Q4_K_M", "phi-4", "crates/darkmux-doctor/src/lib.rs"],
  // A file extension (and the GGUF quant tag before it).
  ["lmstudio-community/phi-4-GGUF/phi-4-Q4_K_M.gguf", "phi-4", "crates/darkmux-doctor/src/lib.rs"],
  ["gemma-vision-Q4_K_M.gguf", "gemma-vision", "crates/darkmux-profiles/src/gestalt_host/gguf_facts.rs"],
  ["mmproj-gemma-vision-BF16.gguf", "mmproj-gemma-vision", "crates/darkmux-profiles/src/gestalt_host/gguf_facts.rs"],
  ["phi-4-mini.gguf", "phi-4-mini", "crates/darkmux-profiles/src/gestalt_host/gguf_facts.rs"],
  ["devstral.gguf", "devstral", "crates/darkmux-profiles/src/gestalt_host/gguf_facts.rs"],
  // Generic tuning tags: dropped at the end; kept where a dated variant
  // follows, since the bare "qwen3-4b-2507" is another model (its thinking
  // twin shares the date).
  ["qwen3-4b-instruct", "qwen3-4b", "crates/darkmux-gestalt/src/planner.rs"],
  ["darkmux:qwen3-4b-instruct-2507", "qwen3-4b-instruct-2507", "crates/darkmux-crew/src/dispatch_internal.rs"],
  ["darkmux:qwen/qwen3-4b-2507", "qwen3-4b-2507", "crates/darkmux-crew/src/concurrent_dispatch.rs"],
  // Already the relevant part: unchanged.
  ["darkmux:qwen3.5-122b-a10b", "qwen3.5-122b-a10b", "docs/demo/demo-fleet.json"],
  ["Llama-3.3-70B", "Llama-3.3-70B", "crates/darkmux-profiles/src/model_ledger.rs"],
  ["Mistral-Large-2-123B", "Mistral-Large-2-123B", "crates/darkmux-profiles/src/model_ledger.rs"],
  ["gpt-4o-2024-08-06", "gpt-4o-2024-08-06", "crates/darkmux-crew/src/run_record.rs"],
  ["gemma-4-e4b", "gemma-4-e4b", "docs/agents/DOCTRINE.md"],
  ["darkmux:qwen-layout", "qwen-layout", "tests/parity/lib/layout-fixture.js"],
];

describe("modelShortName (#3554)", () => {
  it.each(TABLE)("%s -> %s (%s)", (id, short) => {
    expect(modelShortName(id)).toBe(short);
  });

  // Synthetic cases for the rules' edges, beside the real ids above.
  it("drops the chat and it tuning tags at the end", () => {
    expect(modelShortName("llama-3-8b-chat")).toBe("llama-3-8b");
    expect(modelShortName("gemma-3-27b-it")).toBe("gemma-3-27b");
  });
  it("drops the other quant tags", () => {
    expect(modelShortName("model-7b-int4")).toBe("model-7b");
    expect(modelShortName("model-7b-fp16")).toBe("model-7b");
    expect(modelShortName("model-7b-bf16")).toBe("model-7b");
    expect(modelShortName("model-7b-q8_0")).toBe("model-7b");
    expect(modelShortName("model-7b-dwq")).toBe("model-7b");
    expect(modelShortName("model-7b-uint8")).toBe("model-7b");
    expect(modelShortName("model-7b-f16")).toBe("model-7b");
    expect(modelShortName("model-7b-nvfp4")).toBe("model-7b");
    expect(modelShortName("model-7b-IQ4_XS")).toBe("model-7b");
    expect(modelShortName("model-7b-awq")).toBe("model-7b");
    expect(modelShortName("model-7b-gptq")).toBe("model-7b");
    expect(modelShortName("model-7b-exl2")).toBe("model-7b");
    expect(modelShortName("model-7b.safetensors")).toBe("model-7b");
  });
  it("keeps a tuning tag that is all that tells the model apart", () => {
    expect(modelShortName("deepseek-chat")).toBe("deepseek-chat");
  });
  it("never shortens a name to nothing: the first part stays", () => {
    expect(modelShortName("mlx")).toBe("mlx");
    expect(modelShortName("darkmux:instruct")).toBe("instruct");
  });
  it("is empty for no model", () => {
    expect(modelShortName("")).toBe("");
  });
});

// (#3554 review) The one last-segment cut, shared by the shortener and the
// servers line (which keeps the rest of the id).
describe("lastPathSegment", () => {
  it("keeps what follows the last slash", () => {
    expect(lastPathSegment("TensorFold/Qwen3.8-27B-MLX-4bit")).toBe("Qwen3.8-27B-MLX-4bit");
    expect(lastPathSegment("flint-int4")).toBe("flint-int4");
  });
});
