/**
 * A machine card's status: the ONE owner of its words, lamp forms and
 * tooltips. The status line is the only place a status appears, and the same
 * words reach the console through `darkmux machine list`: this enum mirrors
 * `CardStatus` in `src/card_status.rs`, and `tests/fixtures/card-status-rows.json`
 * holds the visible words both sides must give for the same rows.
 *
 * `Checking` and `Disconnected` are the page's own states (a source that has
 * not answered yet, a lost daemon); a one-shot CLI read has neither.
 */

import { LampForm } from "../../lib/lamp";

export enum CardStatus {
  Idle = "idle",
  /** Work is proven and no model is generating. A model's own reading takes
   *  the status line's place instead. */
  Running = "running",
  /** The card was read, so the machine is up; its flow stream does not reach this hub. */
  OnlineNotStreaming = "online_not_streaming",
  /** Nothing reaches this hub and the card could not be read either. */
  NotStreaming = "not_streaming",
  Offline = "offline",
  Checking = "checking",
  Disconnected = "disconnected",
}

/** The status line's word. */
export const STATUS_WORD: Readonly<Record<CardStatus, string>> = {
  [CardStatus.Idle]: "idle",
  [CardStatus.Running]: "dispatch in flight",
  [CardStatus.OnlineNotStreaming]: "online",
  [CardStatus.NotStreaming]: "not streaming",
  [CardStatus.Offline]: "offline",
  [CardStatus.Checking]: "checking…",
  [CardStatus.Disconnected]: "disconnected",
};

/** What the count line says in place of a count: an online machine whose
 *  activity cannot be shown here says why under its status. */
export function secondLineOf(status: CardStatus): string | null {
  return status === CardStatus.OnlineNotStreaming ? "not streaming" : null;
}

/** The lamp's form says what kind of fact the word is: offline is dim, work is
 *  filled, proven idle is hollow, and every word that is no reading is dashed. */
export function lampOf(status: CardStatus): LampForm {
  switch (status) {
    case CardStatus.Offline:
      return LampForm.Off;
    case CardStatus.Running:
      return LampForm.Filled;
    case CardStatus.Idle:
      return LampForm.Hollow;
    case CardStatus.OnlineNotStreaming:
    case CardStatus.NotStreaming:
    case CardStatus.Checking:
    case CardStatus.Disconnected:
      return LampForm.Dashed;
  }
}

/** The status line's tooltip: the word's reason. `note` is the view's typed
 *  reason for a card it could not read ("not listening"); `null` means the
 *  card was read. `undefined` when the word needs none. */
export function statusReason(note: string | null, status: CardStatus): string | undefined {
  switch (status) {
    case CardStatus.Offline:
      return `offline: ${note ?? "its presence beat stopped"}`;
    case CardStatus.OnlineNotStreaming:
      return "online · not streaming: its flow stream doesn't reach this hub, so its activity can't be shown here.";
    case CardStatus.NotStreaming:
      return `not streaming: ${note ?? "its card couldn't be read"}`;
    case CardStatus.Disconnected:
      return "disconnected: this page lost its daemon";
    case CardStatus.Idle:
    case CardStatus.Running:
    case CardStatus.Checking:
      return undefined;
  }
}
