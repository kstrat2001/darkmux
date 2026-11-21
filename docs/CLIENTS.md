# Using the darkmux client API

`darkmux client serve` is the API an app (Mux Bot, or your own) uses to pair with a Mac and run work on it. This page is the order of calls and what to do on each outcome. The routes, request bodies, replies and refusal codes themselves are in the OpenAPI document this page opens (`GET /client/openapi.json`), and this page does not repeat them.

## Where an app reaches it

- **On the Mac:** `darkmux client serve` listens on 127.0.0.1 only. An app on the same Mac connects there.
- **From another device on the tailnet:** when the Mac's operator has turned remote use on, the same API is also at the Mac's tailnet address, through Tailscale. An app on the Mac learns it from `GET /client/whoami`: when `remote` says `available`, its `url` is the address to hand the other device (a QR code, a share sheet).
- Anything else (another proxy, or a web page, which sends an Origin header) is refused `not_local`, or `not_forwarded` while the tailnet address is up.

## Pair on the Mac

1. Make a random pair secret and keep it in memory. Send `POST /client/pair` with a code your app shows its user, a `device_name`, and that secret. The reply names a request. The operator compares the code character for character, so it is refused (`bad_request`) when it holds an invisible, formatting or right-to-left character; use letters, digits and dashes. A `device_name` with an invisible formatting character (a bidirectional control or mark, a zero-width character) is refused too.
2. Show the code. A Mux Bot signed by a verified team is approved at once. Any other app waits for the operator, who approves it in a dialog on the Mac or with `darkmux client approve --code <CODE>`.
3. Poll `GET /client/pair/{request_id}` with the secret in `X-Pair-Secret`:
   - `pending`: keep polling.
   - `approved`: the reply carries the token, this once only. Store it (below) before anything else.
   - `denied`: stop, and tell the user the Mac said no.
   - `expired`: no one approved it in time. Start again from step 1.
   - `not_found`: the request is gone (the wrong secret, a token already collected, or a request expired long ago). Start again.
4. A pairing request answered `too_many_requests` found the queue full. Wait, then start again.

## Pair from another device over the tailnet

Gate this on the app on the Mac: offer it only when that app's `GET /client/whoami` lists `remote_requests` in `capabilities` and `remote` says `available`.

The calls are the ones above, sent to `url`. What differs:

- **It always waits for the operator, who compares your code.** Nothing on the device can approve it. The dialog and `darkmux client approve` show the tailnet user and device the request names, and tell the operator to approve only if that device shows your code. A program on the Mac can send a request that names any device, so the code on your screen is what tells the operator the request is yours: show it large, from the moment the pairing starts until it ends.
- **Only the Mac's own tailnet user pairs.** A request from another user of a shared tailnet is refused `remote_user_not_owner`, naming the user who may. When Tailscale on the Mac cannot say who that is, the request answers `unavailable`: try again later.
- **Tailscale must confirm the user.** The user and device recorded come from Tailscale, never from the body, and Tailscale's own record of the device must name the same user as the request: otherwise it is refused `remote_identity_unconfirmed`. A device logged in to Tailscale with a tag has no user and is refused `remote_identity_missing`: log it in as a user. When Tailscale on the Mac cannot say which device sent the request, it is refused `remote_device_unknown`: try again later.
- **A device has at most two pairing requests waiting.** A third is refused `too_many_requests` (it never replaces an earlier one): wait for one to end.
- **Poll from the same device, as the same user.** A poll from another user is refused `remote_identity_mismatch`, from another device `remote_device_mismatch`, and neither uses up the token.

## Keep the token

- Store it in the platform's secure store (the Keychain on Apple platforms), never in a log, a file or a URL. Send it as an HTTP bearer token on every route but the two pairing ones.
- A device paired over the tailnet is answered through `url` only as the tailnet user and from the device it paired as. From another of the user's devices it is refused `remote_device_mismatch`, as another user `remote_identity_mismatch`. Tell the user which, and do not discard the token.
- The token is still a bearer token: it is not usable only from that device. Sent from a program on the Mac itself, it is answered as an app on the Mac. Guard it as you would a password.
- `not_paired_remotely` means the token was paired on the Mac, so it works there only. Pair the device over the tailnet.
- `unauthorized` means darkmux does not know the token: the operator revoked it (`darkmux client revoke`), on the Mac and remotely at once. Delete it and pair again.
- Pairing again makes a new client with a new token. The old one keeps working until the operator revokes it, and its workspaces stay with it: the operator gives one to the new client with `darkmux client claim`.

## Check what this darkmux can do

After pairing, call `GET /client/whoami` and gate every feature on its `capabilities`, never on `version`. A darkmux with no `capabilities` at all predates the list. A capability says what the build can do, not what this Mac allows: a preview may still be refused `preview_disabled`, and remote use is on only while `remote` says `available`.

## Read a refusal

Every refusal carries a `code` and an `error`. Branch on the `code`; show the `error` to a person; never parse it. `unavailable` means not now (the Mac's settings could not be read, or `darkmux client serve` is stopping): send the request again later. `too_many_requests` means slow down. A code your app does not know comes from a newer darkmux: handle it by the HTTP status class.

## Find an engine, or add one

Gate this on `engines` in `capabilities`.

1. Call `GET /client/engines`. It lists each engine darkmux found installed on the Mac in `detected`, and `usable` says whether any of them can run work. darkmux recommends none: when `usable` is false, which engine to suggest is your app's choice.
2. Each entry says which `engine` it is, its `version`, its `state` (`usable`, `unusable`, or `unknown` when a probe failed or ran out of time), a `reason` for the last two, whether it is `configured`, and the `models` it holds. A model's `max_context` is the largest context the engine states for it, when it states one.
3. An entry with `can_add` can be added, by an app on the Mac or by a device paired over the tailnet, through `url`. Ask your user first, then send `POST /client/engines` with its `engine`. Name a `model` when it holds more than one, and an `n_ctx` when the model states no `max_context` (that one is used otherwise). The request is the user's confirmation: darkmux writes the Mac's profile registry at once, and records the add in the `darkmux client serve` log, naming your client and whether it came from the Mac or over the tailnet (with the tailnet user and device). The reply names the new `profile`, which `POST /client/work` can run on.
4. A refusal says why: `engine_unavailable` (not installed, or not usable as it is), `conflict` (configured already, including by another add that landed first, or the registry changed meanwhile: list again), `bad_request` (a `model` or `n_ctx` it cannot take, or an engine that needs nothing added), `unprocessable` (the registry cannot be edited safely; the operator fixes it on the Mac).
5. When the Mac's operator has set which profiles apps may use, the new profile is listed by `GET /client/profiles` only once they add it there.

## Run work

1. Optionally list what may run with `GET /client/profiles`.
2. Make or reuse a workspace with `POST /client/workspaces`, and name it in the run.
3. Start the run with `POST /client/work`. The reply names it.
4. Poll `GET /client/work/{work_id}` until its `status` is `done`, `error` or `cancelled`. While it is `running`, `activity` may say `starting_model` or `queued`: show that rather than a stall.
5. To stop it, `POST /client/work/{work_id}/cancel`, then poll on through `cancelling` until it ends.
6. Whatever a run changed is committed when it ends, however it ends. To take a finished run's changes back, `POST /client/work/{work_id}/undo` reverts them in a new commit; it is refused while a later run changed the same files, or another run works in the same workspace. `POST /client/work/{work_id}/keep` and `POST /client/work/{work_id}/discard` are for older apps: keep only reports the commit, and discard is an Undo.
7. A follow-up run in the same workspace names the run it `continues` from.
8. Read the result while `darkmux client serve` still holds it: a run is forgotten an hour after it ends, and at once if `darkmux client serve` restarts. A `not_found` for a run you started means it was forgotten.

## Workspaces and previews

- Look at a workspace with `GET /client/workspaces/{name}` and at what its runs changed with `GET /client/workspaces/{name}/changes`. A hand edit goes in with `POST /client/workspaces/{name}/edit`, an image with `POST /client/workspaces/{name}/image`.
- Remove one with `DELETE /client/workspaces/{name}`. It is refused while a run, an Undo or a preview holds it, and refused `unsaved_commits` when it holds commits kept nowhere else, until the user agrees to `force` it.
- Start a preview with `POST /client/workspaces/{name}/preview`, poll `GET /client/workspaces/{name}/preview` until it is ready or has failed, and stop it with `DELETE /client/workspaces/{name}/preview`. A preview also stops when it sits idle, when the app is revoked and when `darkmux client serve` stops, so read its status instead of assuming it still runs.
- A workspace with no preview recipe is refused `no_recipe`. For a cloned repository whose user approves one, `PUT /client/workspaces/{name}/recipe` sets it, when `recipe_set` is listed.
- A preview's address is on the Mac's loopback. A device on the tailnet opens it through a link instead (below).

## See a preview from another device over the tailnet

Gate this on `remote_preview` in `capabilities`, with `remote` `available`. A device paired over the tailnet sees one of its previews in its browser, through `url`:

1. Start the preview and poll it until it is ready, as above.
2. Send `POST /client/workspaces/{name}/preview/link` through `url`, with the device's token. The reply's `url` is a link to that preview for this device: it opens once, within `expires_in_secs`. Open it in the device's browser at once, and never keep or log it. From an app on the Mac it is refused `tailnet_only` (the preview's own address serves it), and for a preview that is not ready, `conflict`.
3. The browser opens the link (`GET /client/preview-link/{code}`). darkmux checks the request comes from the tailnet user and device the link was made for, sets a session cookie in that browser and sends it on to the root of `url`, where the preview is. Every path outside /client/ there is the preview's, its WebSocket upgrades included, so live reload works when the preview's reload socket uses the app port.
4. A link opened a second time, or too late, answers `not_found`; one whose preview stopped or restarted since it was made, or whose device was revoked, `conflict`. From another device or user it answers as a token would (`remote_device_mismatch`, `remote_identity_mismatch`), and is used up all the same. Ask for a new link.
5. The session lasts while its preview runs. It ends when the preview stops or restarts, when the device is revoked, when previews are turned off on the Mac, and when `darkmux client serve` stops: the preview's pages then answer `no_preview_session`, and an open WebSocket closes, and an answer still streaming is cut, within a second. Ask for a new link and open it.

What that means for a preview:

- A browser holds one session for the Mac's address, so it shows one preview at a time: opening a link to another preview replaces it.
- The preview's own paths under /client/ answer as darkmux's routes, not the preview's.
- Its server gets requests with its own loopback address as their host and origin, without the device's token, the session cookie or Tailscale's headers. The rest of what the browser sends reaches it as sent: the referrer and fetch metadata name the tailnet address, and the browser's other cookies for the Mac's address (another preview's, or a service's on another port of that address) go with every request.
- Its answers cannot set the session cookie, nor a cookie for other hosts (one with a Domain). The proxy cannot see cookies the preview's own scripts set in the browser (document.cookie). Its answers are never cached (each is sent no-store, whatever the preview says), and it cannot register a service worker (refused `forbidden`).
- Only the browser's own requests to the Mac's address reach it: one from another origin or another site, a WebSocket included, is refused `forbidden`. Every tailnet address is the same site to a browser, so this, not the cookie, is what keeps another tailnet page out.
- A page of the preview counts as asking about it, so the idle check does not stop a preview while it is open.
- At most six of a page's requests reach the preview at once (the rest wait their turn, as a browser's own connection limit makes them), and at most two WebSockets stay open (another answers `too_many_requests`). A request the preview does not begin to answer within three minutes answers `unavailable`.

**Previews share one origin.** Every preview is served at the same address, so the browser treats them as one site. Opening a link asks the browser to clear that address's cache and storage (the Clear-Site-Data header, which browsers support unevenly), but a tab still open on preview A, or storage the browser keeps for the address, can still reach preview B once B's session is set: A's page would then be talking to B. Show each preview in a web view of its own, with a data store of its own (on Apple platforms, a web view whose data store is made for that preview with WKWebsiteDataStore(forIdentifier:)), and close a preview's web view before opening another's link.
