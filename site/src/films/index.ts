import { AskFirst, ASK_FRAMES } from "./AskFirst"
import { Logbook, LOGBOOK_FRAMES } from "./Logbook"
import { Room, ROOM_FRAMES } from "./Room"

export const FILMS = {
  room: { component: Room, frames: ROOM_FRAMES },
  logbook: { component: Logbook, frames: LOGBOOK_FRAMES },
  ask: { component: AskFirst, frames: ASK_FRAMES },
} as const
export type FilmId = keyof typeof FILMS
