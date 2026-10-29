import { cubicBezier, Easing, FadeIn, LinearTransition, ReduceMotion } from 'react-native-reanimated'

// Two durations and one curve for every animation, the same as the theme's on the web: no bounce, no overshoot.
/** Presses and colour changes. */
export const quick = 150
/** Anything that appears, leaves, folds or moves. */
export const settle = 200
/** The theme's `--ease-out`. */
export const easeOut = Easing.bezier(0.23, 1, 0.32, 1)
/** The same curve for `transition*` and `animation*` styles. */
export const cssEaseOut = cubicBezier(0.23, 1, 0.32, 1)
/** `withTiming` configs; with Reduce Motion they jump to the end. */
export const tap = { duration: quick, easing: easeOut, reduceMotion: ReduceMotion.System }
export const glide = { duration: settle, easing: easeOut, reduceMotion: ReduceMotion.System }
/** `entering` for something that appears in place. */
export const fadeIn = FadeIn.duration(settle).easing(easeOut).reduceMotion(ReduceMotion.System)
/** `layout` for something that shifts as what is around it comes or goes. */
export const move = LinearTransition.duration(settle).easing(easeOut).reduceMotion(ReduceMotion.System)
