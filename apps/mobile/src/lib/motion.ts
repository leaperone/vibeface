import { arcTokens } from '@vibeface/theme/tokens'
import { cubicBezier, Easing, FadeIn, FadeOut, LinearTransition, ReduceMotion } from 'react-native-reanimated'

export const quick = arcTokens.duration.fast
export const settle = arcTokens.duration.standard
export const easeOut = Easing.bezier(...arcTokens.ease.standard)
export const cssEaseOut = cubicBezier(...arcTokens.ease.standard)
export const tap = { duration: arcTokens.duration.instant, easing: easeOut, reduceMotion: ReduceMotion.System }
export const glide = { duration: settle, easing: easeOut, reduceMotion: ReduceMotion.System }
export const fadeIn = FadeIn.duration(settle).easing(Easing.bezier(...arcTokens.ease.enter)).reduceMotion(ReduceMotion.System)
export const fadeOut = FadeOut.duration(arcTokens.duration.exit).easing(Easing.bezier(...arcTokens.ease.exit)).reduceMotion(ReduceMotion.System)
export const move = LinearTransition.duration(settle).easing(easeOut).reduceMotion(ReduceMotion.System)
