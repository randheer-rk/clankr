import { StrictMode } from "react"
import { createRoot } from "react-dom/client"
import { Player } from "@remotion/player"
import { FILMS, type FilmId } from "./films"
import { H, W } from "./films/kit"

// ?film=room&f=120 renders one film, paused on one frame (for checking stills)
const q = new URLSearchParams(location.search)
const film = q.get("film") as FilmId | null
const root = createRoot(document.getElementById("root")!)
if (film && FILMS[film]) {
  const { component, frames } = FILMS[film]
  document.body.style.margin = "0"
  root.render(<Player component={component} durationInFrames={frames} compositionWidth={W} compositionHeight={H} fps={30} initialFrame={+(q.get("f") ?? 0)} acknowledgeRemotionLicense style={{ width: "100vw", height: "100vh" }} />)
} else {
  import("./App").then(({ App }) => root.render(<StrictMode><App /></StrictMode>))
}
