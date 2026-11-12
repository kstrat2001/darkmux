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

/** A model id's last path segment (`TensorFold/Qwen3.8-27B` → `Qwen3.8-27B`):
 *  the publisher or path prefix dropped, the rest kept as it is. */
export function lastPathSegment(id: string): string {
  return id.slice(id.lastIndexOf("/") + 1);
}

/** The id's model name: no namespace, no publisher or path prefix, no file
 *  extension, no storage parts. A tuning tag (`instruct`, `chat`, `it`)
 *  stays: it names a different model, not a way of storing one. The first
 *  part always stays, so a name never shortens to nothing. */
export function modelShortName(id: string): string {
  const base = id.replace(NAMESPACE, "");
  const file = lastPathSegment(base).replace(EXTENSION, "");
  const [first, ...rest] = file.split("-");
  return [first, ...rest.filter((part) => !STORAGE.test(part))].join("-");
}

/** (#3554) A model's name on a card that may show several executions: its
 *  short name, unless another execution's different id shortens to the same
 *  (the A/B runs darkmux makes, `-turboquant-mlx` vs `-mlx`). Then its last
 *  path segment, which keeps the storage parts; and when even that clashes
 *  (only the publisher differs), the id without its namespace. */
export function cardModelName(id: string, siblings: readonly string[]): string {
  const bare = id.replace(NAMESPACE, "");
  const others = siblings.map((o) => o.replace(NAMESPACE, "")).filter((o) => o !== bare);
  const short = modelShortName(bare);
  if (!others.some((o) => modelShortName(o) === short)) return short;
  const segment = lastPathSegment(bare);
  return others.some((o) => lastPathSegment(o) === segment) ? bare : segment;
}
