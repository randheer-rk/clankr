import { useEffect, useRef, useState } from "react"
import { Player, type PlayerRef } from "@remotion/player"

/**
 * A Remotion composition, looping in the page. It plays only while it's on screen,
 * and holds a still frame for people who prefer reduced motion.
 */
export function Scene({ component, frames, still, label }: { component: React.ComponentType<{ tall?: boolean }>; frames: number; still: number; label: string }) {
  const wrap = useRef<HTMLDivElement>(null)
  const player = useRef<PlayerRef>(null)
  // phones get the portrait cut of each scene, so nothing shrinks past reading size
  const [tall] = useState(() => matchMedia("(max-width: 760px)").matches)
  const [w, h] = tall ? [470, 660] : [940, 340]

  useEffect(() => {
    const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches
    if (reduce) {
      player.current?.seekTo(still)
      return
    }
    const io = new IntersectionObserver(([e]) => (e.isIntersecting ? player.current?.play() : player.current?.pause()), { threshold: 0.25 })
    if (wrap.current) io.observe(wrap.current)
    return () => io.disconnect()
  }, [still])

  return (
    <div ref={wrap} className="scene" role="img" aria-label={label}>
      <Player
        ref={player}
        component={component}
        durationInFrames={frames}
        compositionWidth={w}
        compositionHeight={h}
        inputProps={{ tall }}
        fps={30}
        loop
        autoPlay={!matchMedia("(prefers-reduced-motion: reduce)").matches}
        initialFrame={matchMedia("(prefers-reduced-motion: reduce)").matches ? still : 0}
        controls={false}
        clickToPlay={false}
        acknowledgeRemotionLicense
        style={{ width: "100%", aspectRatio: `${w} / ${h}` }}
      />
    </div>
  )
}
