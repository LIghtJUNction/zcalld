#import <Foundation/Foundation.h>
#import <UserNotifications/UserNotifications.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>

// Process-lifetime state remains valid if an OS completion arrives after timeout.
static atomic_bool completed = false;
static atomic_bool accepted = false;

static NSString *const Identity = @"dev.zcalld.desktop";
static int parse(NSData *data, NSString **title, NSString **body) {
    if (data.length > 2048) return 5;
    id value = [NSJSONSerialization JSONObjectWithData:data options:0 error:NULL];
    if (![value isKindOfClass:[NSDictionary class]]) return 5;
    id t = value[@"title"], b = value[@"body"];
    if (![t isKindOfClass:[NSString class]] || ![b isKindOfClass:[NSString class]] ||
        [t length] > 160 || [b length] > 1600) return 5;
    *title = t; *body = b; return 0;
}
static BOOL waitUntil(BOOL (^finished)(void), NSTimeInterval seconds) {
    NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:seconds];
    while (!finished() && [deadline timeIntervalSinceNow] > 0) {
        [[NSRunLoop currentRunLoop] runMode:NSDefaultRunLoopMode beforeDate:[NSDate dateWithTimeIntervalSinceNow:0.01]];
    }
    return finished();
}
int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc != 2) return 5;
        NSString *mode = [NSString stringWithUTF8String:argv[1]];
        if ([mode isEqualToString:@"--self-test"]) {
            NSString *title, *body;
            NSData *data = [@"{\"title\":\"Fixture <tag>\",\"body\":\"Synthetic ✓\"}" dataUsingEncoding:NSUTF8StringEncoding];
            if (parse(data, &title, &body) != 0 || ![title isEqualToString:@"Fixture <tag>"] || ![body isEqualToString:@"Synthetic ✓"]) return 5;
            puts("self-test passed"); return 0;
        }
        if (![mode isEqualToString:@"--notify-stdin"] && ![mode isEqualToString:@"--probe"] && ![mode isEqualToString:@"--request-permission"]) return 5;
        // A raw CLI executable does not acquire an identity by claiming a string.
        if (![[[NSBundle mainBundle] bundleIdentifier] isEqualToString:Identity]) return 4;
        UNUserNotificationCenter *center = [UNUserNotificationCenter currentNotificationCenter];
        if ([mode isEqualToString:@"--request-permission"]) {
            // This entry point is explicit operator action, never called by the plugin.
            atomic_store(&completed, false); atomic_store(&accepted, false);
            [center requestAuthorizationWithOptions:UNAuthorizationOptionAlert | UNAuthorizationOptionSound
                completionHandler:^(BOOL allowed, NSError *error) { atomic_store(&accepted, allowed && error == nil); atomic_store(&completed, true); }];
            if (!waitUntil(^BOOL { return atomic_load(&completed); }, 60.0)) return 5;
            if (!atomic_load(&accepted)) return 3;
            puts("authorized"); return 0;
        }
        atomic_store(&completed, false); atomic_store(&accepted, false);
        [center getNotificationSettingsWithCompletionHandler:^(UNNotificationSettings *settings) {
            atomic_store(&accepted, settings.authorizationStatus == UNAuthorizationStatusAuthorized || settings.authorizationStatus == UNAuthorizationStatusProvisional);
            atomic_store(&completed, true);
        }];
        if (!waitUntil(^BOOL { return atomic_load(&completed); }, 0.20)) return 5;
        if (!atomic_load(&accepted)) return 3;
        if ([mode isEqualToString:@"--probe"]) { puts("available"); return 0; }
        // The parent sends <=2048 bytes then closes stdin; retain at most 2049.
        NSMutableData *input = [NSMutableData data]; uint8_t bytes[256]; size_t n;
        while ((n = fread(bytes, 1, sizeof(bytes), stdin)) > 0) {
            if (input.length + n > 2048) return 5;
            [input appendBytes:bytes length:n];
        }
        NSString *title, *body; if (parse(input, &title, &body) != 0) return 5;
        UNMutableNotificationContent *content = [UNMutableNotificationContent new];
        content.title = title; content.body = body;
        // No raw action, launch URL, account name or userInfo payload.
        UNNotificationRequest *request = [UNNotificationRequest requestWithIdentifier:[[NSUUID UUID] UUIDString] content:content trigger:nil];
        atomic_store(&completed, false); atomic_store(&accepted, false);
        [center addNotificationRequest:request withCompletionHandler:^(NSError *error) { atomic_store(&accepted, error == nil); atomic_store(&completed, true); }];
        if (!waitUntil(^BOOL { return atomic_load(&completed); }, 0.35) || !atomic_load(&accepted)) return 5;
        puts("accepted"); return 0;
    }
}
