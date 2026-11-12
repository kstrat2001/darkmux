/**
 * (#3554) A model id as a narrow surface names it: only the part that says
 * which model it is (family, version, size, an active-parameter marker such
 * as `A3B`), so a one-line slot that ellipsizes cuts nothing that matters.
 * The full id stays wherever there is room (a tooltip). One rule for every
 * surface that shortens a model id; the fleet card's count line is the first.
 */

/** darkmux's namespace on a model it loads (contract 4). */
const NAMESPACE = /^darkmux:/;

/** A model file's extension. */
const EXTENSION = /\.(gguf|safetensors)$/i;

/** One `-`-separated part that names how the weights are stored, never which
 *  model: bit widths (`4bit`), integer and float formats (`int4`, `bf16`,
 *  `fp16`, `mxfp4`), GGUF quant tags (`q4_k_m`, `q8_0`, `iq4_xs`), and the
 *  runtime or method a conversion was made for (`mlx`, `gguf`, `dwq`,
 *  `turboquant`, ...). Dropped wherever it sits. */
const STORAGE = /^(\d+bit|u?int\d+|b?f\d+|(mx|nv)?fp\d+|i?q\d+(_[a-z0-9]+)*|mlx|gguf|dwq|awq|gptq|exl2|turboquant)$/i;

/** A generic tuning tag. Dropped only at the end, and only when a part
 *  beyond the family stays: inside a name, or alone after it, it can be what
 *  tells two models apart (`qwen3-4b-instruct-2507` and its thinking twin
 *  share `qwen3-4b-2507`; `deepseek-chat` is not `deepseek`). */
const TUNING = /^(instruct|chat|it)$/i;

/** The id's model name: no namespace, no publisher or path prefix, no file
 *  extension, no storage parts, no trailing tuning tag. The first part always
 *  stays, so a name never shortens to nothing. */
export function modelShortName(id: string): string {
  const base = id.replace(NAMESPACE, "");
  const file = base.slice(base.lastIndexOf("/") + 1).replace(EXTENSION, "");
  const [first, ...rest] = file.split("-");
  const kept = rest.filter((part) => !STORAGE.test(part));
  while (kept.length > 1 && TUNING.test(kept[kept.length - 1])) kept.pop();
  return [first, ...kept].join("-");
}
