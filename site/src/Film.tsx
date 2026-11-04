import { useEffect, useRef, useState } from "react"
import { Player, type PlayerRef } from "@remotion/player"
import { FILMS, type FilmId } from "./films"
import { H, W } from "./films/kit"

const still = matchMedia("(prefers-reduced-motion: reduce)").matches

/** A film that plays while it's on screen and pauses when it isn't. */
export function Film({ id }: { id: FilmId }) {
  const box = useRef<HTMLDivElement>(null)
  const player = useRef<PlayerRef>(null)
  const [seen, setSeen] = useState(false)
  const { component, frames } = FILMS[id]

  useEffect(() => {
    const io = new IntersectionObserver(([e]) => {
      if (e.isIntersecting) setSeen(true)
      if (!still) e.isIntersecting ? player.current?.play() : player.current?.pause()
    }, { threshold: 0.35 })
    io.observe(box.current!)
    return () => io.disconnect()
  }, [])

  return (
    <div ref={box} className="film" style={{ aspectRatio: `${W} / ${H}` }}>
      {seen && (
        <Player ref={player} component={component} durationInFrames={frames} compositionWidth={W} compositionHeight={H} fps={30}
          autoPlay={!still} loop={!still} initialFrame={still ? frames - 40 : 0} acknowledgeRemotionLicense
          style={{ width: "100%", height: "100%" }} />
      )}
    </div>
  )
}
