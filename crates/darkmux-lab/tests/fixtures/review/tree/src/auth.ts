// (#2310 P4c-2b fixture) unnamed-predicate, reused from crawl (confirm=mod).
export function checkAccess(user: { role: string; active: boolean }, resource: { owner: string; shared: boolean }) {
  if ((user.role === "admin" && user.active) || (resource.shared && resource.owner === user.role)) {
    return true;
  }
  return false;
}
