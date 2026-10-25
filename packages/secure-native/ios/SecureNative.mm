#import "SecureNative.h"

namespace uniffi_generated {
    using namespace facebook::react;
    class JSI_EXPORT NativeSecureNativeSpecJSI : public ObjCTurboModule {
    public:
        NativeSecureNativeSpecJSI(const ObjCTurboModule::InitParams &params);
        std::shared_ptr<CallInvoker> callInvoker;
    };

    static facebook::jsi::Value __hostFunction_SecureNative_installRustCrate(facebook::jsi::Runtime& rt, TurboModule &turboModule, const facebook::jsi::Value* args, size_t count) {
        auto& tm = static_cast<NativeSecureNativeSpecJSI&>(turboModule);
        auto jsInvoker = tm.callInvoker;
        uint8_t result = vibeface_securenative::installRustCrate(rt, jsInvoker);
        return facebook::jsi::Value(rt, result);
    }
    static facebook::jsi::Value __hostFunction_SecureNative_cleanupRustCrate(facebook::jsi::Runtime& rt, TurboModule &turboModule, const facebook::jsi::Value* args, size_t count) {
        uint8_t result = vibeface_securenative::cleanupRustCrate(rt);
        return facebook::jsi::Value(rt, result);
    }

    NativeSecureNativeSpecJSI::NativeSecureNativeSpecJSI(const ObjCTurboModule::InitParams &params)
        : ObjCTurboModule(params), callInvoker(params.jsInvoker) {
            this->methodMap_["installRustCrate"] = MethodMetadata {1, __hostFunction_SecureNative_installRustCrate};
            this->methodMap_["cleanupRustCrate"] = MethodMetadata {1, __hostFunction_SecureNative_cleanupRustCrate};
    }
} // namespace uniffi_generated

@implementation SecureNative
RCT_EXPORT_MODULE()

- (NSNumber *)installRustCrate {
    @throw [NSException exceptionWithName:@"UnreachableException"
                        reason:@"This method should never be called."
                        userInfo:nil];
}

- (NSNumber *)cleanupRustCrate {
    @throw [NSException exceptionWithName:@"UnreachableException"
                        reason:@"This method should never be called."
                        userInfo:nil];
}

- (std::shared_ptr<facebook::react::TurboModule>)getTurboModule:
    (const facebook::react::ObjCTurboModule::InitParams &)params
{
    return std::make_shared<uniffi_generated::NativeSecureNativeSpecJSI>(params);
}

@end
