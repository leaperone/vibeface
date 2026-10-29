import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { ReplaceRecovery } from '@/features/recovery/recovery'

export default function Recovery() {
  return <KeyboardAwareScrollView bottomOffset={24} contentInsetAdjustmentBehavior="automatic" keyboardShouldPersistTaps="handled" contentContainerClassName="w-full max-w-3xl self-center p-4 pb-10">
    <ReplaceRecovery />
  </KeyboardAwareScrollView>
}
