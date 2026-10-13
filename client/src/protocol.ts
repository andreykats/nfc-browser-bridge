// NFC Browser Bridge wire protocol, version 1. Mirrors docs/PROTOCOL.md exactly.

export const PROTOCOL_VERSION = 1;
export const DEFAULT_URL = "ws://127.0.0.1:38241/v1";

export type ReaderKind = "picc" | "sam" | "other";
export type ServiceStatus = "ready" | "unavailable";

export interface CardInfo {
  /** Uppercase hex, no separators; null if the card did not answer FF CA. */
  uid: string | null;
  atr: string;
}

export interface Reader {
  name: string;
  kind: ReaderKind;
  card: CardInfo | null;
}

// ---- Server events ----

export interface HelloEvent {
  event: "hello";
  protocol: number;
  app: "nfc-browser-bridge";
  version: string;
  service: ServiceStatus;
  readers: Reader[];
}

export interface ReadersEvent {
  event: "readers";
  service: ServiceStatus;
  readers: Reader[];
}

export interface CardPresentEvent {
  event: "card_present";
  reader: string;
  uid: string | null;
  atr: string;
}

export interface CardRemovedEvent {
  event: "card_removed";
  reader: string;
}

export type ServerEvent = HelloEvent | ReadersEvent | CardPresentEvent | CardRemovedEvent;

// ---- Client requests ----

export type RequestId = string | number;

export interface PingRequest {
  type: "ping";
}
export interface ListReadersRequest {
  type: "list_readers";
}
export interface TransmitRequest {
  type: "transmit";
  reader: string;
  apdus: string[];
  stopOnError?: boolean;
  unsafe?: boolean;
}
export interface ResetRequest {
  type: "reset";
  reader: string;
}

export type Request = PingRequest | ListReadersRequest | TransmitRequest | ResetRequest;

export interface ApduResponse {
  /** Response body without the status word, uppercase hex. */
  data: string;
  /** Status word, e.g. "9000". */
  sw: string;
}

export interface ResultMap {
  ping: Record<string, never>;
  list_readers: { service: ServiceStatus; readers: Reader[] };
  transmit: { responses: ApduResponse[] };
  reset: { atr: string };
}

// ---- Responses ----

export type ErrorCode =
  | "bad_request"
  | "protocol_mismatch"
  | "no_such_reader"
  | "no_card"
  | "card_removed"
  | "sharing_violation"
  | "transmit_failed"
  | "blocked_write"
  | "pcsc_unavailable"
  | "busy";

export interface ErrorBody {
  code: ErrorCode;
  message: string;
  /** Raw PC/SC status, e.g. "0x8010000C". */
  pcsc?: string;
  /** transmit only: APDUs that completed before a PC/SC failure. */
  completed?: number;
}

export interface OkResponse<R = unknown> {
  id: RequestId | null;
  ok: true;
  result: R;
}

export interface ErrResponse {
  id: RequestId | null;
  ok: false;
  error: ErrorBody;
}

export type Response = OkResponse | ErrResponse;

export const LIMITS = {
  maxMessageBytes: 1024 * 1024,
  maxApdusPerBatch: 600,
  minApduBytes: 4,
  maxApduBytes: 261,
  maxInFlight: 32,
} as const;
