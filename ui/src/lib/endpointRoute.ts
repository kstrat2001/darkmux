/**
 * (#2834, #3554) A dispatch record's `endpoint`, the route label darkmux
 * writes for a model reached over HTTP: `<kind>:<host>/<model>`
 * (`darkmux_flow::unmanaged_route_label`). `kind` is `azure` when the host
 * contains "azure" and `openai` for every other host: a guess from the
 * address, not the request format (an Anthropic messages endpoint is labeled
 * `openai:` too). `host` is the address alone (never a scheme, a path or
 * credentials), and the model id may itself hold `/`. One reading for every
 * surface that words a route.
 */

export interface EndpointRoute {
  kind: string;
  host: string;
  model: string;
}

/** The label's parts. A label with no `:` has no kind; one with no `/` names
 *  no model. */
export function parseEndpointRoute(ep: string): EndpointRoute {
  const i = ep.indexOf(":");
  const rest = i >= 0 ? ep.slice(i + 1) : ep;
  const slash = rest.indexOf("/");
  return {
    kind: i >= 0 ? ep.slice(0, i) : "",
    host: slash >= 0 ? rest.slice(0, slash) : rest,
    model: slash >= 0 ? rest.slice(slash + 1) : "",
  };
}

/** The kind by name, as the run page's route line words it. Only `azure` says
 *  something about the host; `openai` is every other host (a local server, an
 *  Anthropic endpoint), so a surface that must not misname one names it by
 *  its address instead. */
export function routeKindLabel(kind: string): string {
  if (kind === "azure") return "Azure OpenAI";
  if (kind === "openai") return "OpenAI";
  return kind || "endpoint";
}
