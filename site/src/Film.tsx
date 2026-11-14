import { useEffect, useRef, useState } from 'react'
import { Player, type PlayerRef } from '@remotion/player'
import { FILMS, type FilmId } from './films'
import { H, W } from './films/kit'

/** Plays when scrolled into view; switches to a portrait layout when narrow. */
export default function Film({ id }: { id: FilmId }) {
  const box = useRef<HTMLDivElement>(null)
  const player = useRef<PlayerRef>(null)
  const [seen, setSeen] = useState(false)
  const [visible, setVisible] = useState(false)
  const [playing, setPlaying] = useState(false)
  const [compact, setCompact] = useState(() => matchMedia('(max-width: 700px)').matches)
  const [reduced] = useState(() => matchMedia('(prefers-reduced-motion: reduce)').matches)
  const { component, frames } = FILMS[id]

  useEffect(() => {
    const io = new IntersectionObserver(([entry]) => {
      setVisible(entry.isIntersecting)
      if (entry.isIntersecting) setSeen(true)
    }, { threshold: .2 })
    const resize = new ResizeObserver(([entry]) => setCompact(entry.contentRect.width < 600))
    io.observe(box.current!)
    resize.observe(box.current!)
    return () => { io.disconnect(); resize.disconnect() }
  }, [])

  useEffect(() => {
    const current = player.current
    if (!current) return
    const play = () => setPlaying(true), pause = () => setPlaying(false)
    current.addEventListener('play', play)
    current.addEventListener('pause', pause)
    return () => { current.removeEventListener('play', play); current.removeEventListener('pause', pause) }
  }, [seen, id])

  useEffect(() => {
    if (visible && !reduced) player.current?.play()
    else player.current?.pause()
  }, [visible, reduced, seen, id])

  return <div ref={box} className="film-shell"><div className="film" data-film={id} style={{ aspectRatio: compact ? '800 / 1000' : `${W} / ${H}` }}>
    {seen && <Player key={id} ref={player} component={component} durationInFrames={frames} compositionWidth={compact ? 800 : W} compositionHeight={compact ? 1000 : H} fps={30}
      clickToPlay spaceKeyToPlayOrPause loop={!reduced} initialFrame={reduced ? frames - 45 : 0} acknowledgeRemotionLicense
      style={{ width: '100%', height: '100%' }} />}
  </div>{seen && <div className="film-controls">
    <button type="button" className="toggle" onClick={() => player.current?.toggle()} aria-label={playing ? 'Pause animation' : 'Play animation'}>{playing ? 'Pause' : 'Play'}</button>
    <button type="button" onClick={() => { player.current?.seekTo(0); player.current?.play() }}>Replay</button>
    <span>13 seconds · Illustrative demo</span>
    <button type="button" onClick={() => player.current?.requestFullscreen()}>Full screen</button>
  </div>}</div>
}
