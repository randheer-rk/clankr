import { useEffect, useRef, type CSSProperties } from 'react'
import mark from '../public/clankr-mark-animated.svg?raw'

/** Inline the original asset so the eyes can track the pointer independently of the blink. */
export function Lockbot({ size = 26 }: { size?: number }) {
  const ref = useRef<HTMLSpanElement>(null)

  useEffect(() => {
    const svg = ref.current!.querySelector('svg')!
    const eyes = svg.querySelector('g')!
    const reduced = matchMedia('(prefers-reduced-motion: reduce)')
    let pointer: { x: number; y: number } | null = null
    let frame = 0

    eyes.querySelector('animateTransform')?.remove()
    eyes.classList.add('lockbot-eyes')

    const draw = () => {
      frame = 0
      if (!pointer || reduced.matches) {
        eyes.style.transform = 'translate(0px, 0px)'
        return
      }
      const matrix = svg.getScreenCTM()
      if (!matrix) return
      const point = new DOMPoint(pointer.x, pointer.y).matrixTransform(matrix.inverse())
      const dx = point.x - 60, dy = point.y - 80.5
      const distance = Math.hypot(dx, dy, 70)
      eyes.style.transform = `translate(${9 * dx / distance}px, ${6 * dy / distance}px)`
    }
    const schedule = () => { if (!frame) frame = requestAnimationFrame(draw) }
    const follow = (event: PointerEvent) => {
      if (event.pointerType !== 'mouse' || reduced.matches) return
      pointer = { x: event.clientX, y: event.clientY }
      schedule()
    }
    const reset = () => { pointer = null; schedule() }
    const preference = () => {
      reset()
      if (reduced.matches) svg.pauseAnimations()
      else svg.unpauseAnimations()
    }

    preference()
    window.addEventListener('pointermove', follow)
    window.addEventListener('scroll', schedule, { passive: true })
    window.addEventListener('resize', schedule)
    window.addEventListener('blur', reset)
    document.addEventListener('pointerleave', reset)
    reduced.addEventListener('change', preference)
    return () => {
      cancelAnimationFrame(frame)
      window.removeEventListener('pointermove', follow)
      window.removeEventListener('scroll', schedule)
      window.removeEventListener('resize', schedule)
      window.removeEventListener('blur', reset)
      document.removeEventListener('pointerleave', reset)
      reduced.removeEventListener('change', preference)
    }
  }, [])

  return <span ref={ref} className="lockbot" aria-hidden="true" style={{ '--lockbot-size': `${size}px` } as CSSProperties} dangerouslySetInnerHTML={{ __html: mark }} />
}
