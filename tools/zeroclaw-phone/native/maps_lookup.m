#import <Foundation/Foundation.h>
#import <MapKit/MapKit.h>
#import "maps_lookup.h"
#include <stdlib.h>
#include <string.h>

static const NSUInteger MAX_RESULTS = 8;
static const NSUInteger MAX_OUTPUT_BYTES = 32768;

static NSString *bounded(NSString *value, NSUInteger limit) {
    if (![value isKindOfClass:[NSString class]] || value.length == 0 ||
        [value lengthOfBytesUsingEncoding:NSUTF8StringEncoding] > limit) return nil;
    return value;
}

static NSDictionary *envelope(NSString *status, NSArray *items, BOOL truncated,
                              NSString *errorCode) {
    NSMutableDictionary *out = [@{@"schema_version": @1, @"source": @"apple_mapkit",
        @"status": status, @"items": items, @"truncated": @(truncated)} mutableCopy];
    if (errorCode) out[@"error_code"] = errorCode;
    return out;
}

static int emit(NSDictionary *value, uint8_t **output, size_t *outputLen, int code) {
    NSError *error = nil;
    NSData *data = [NSJSONSerialization dataWithJSONObject:value
        options:NSJSONWritingSortedKeys error:&error];
    if (!data || error || data.length > MAX_OUTPUT_BYTES) return 70;
    uint8_t *bytes = malloc(data.length);
    if (!bytes) return 70;
    memcpy(bytes, data.bytes, data.length);
    *output = bytes;
    *outputLen = data.length;
    return code;
}

// MKPlacemark remains the public fallback for macOS 15/16. The newer address
// properties are used on 26+, so there is no deprecated execution on this Mac.
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
static NSString *publicAddress(MKMapItem *item) {
    if (@available(macOS 26.0, *)) return bounded(item.address.fullAddress, 2048);
    return bounded(item.placemark.title, 2048);
}
static NSString *publicCity(MKMapItem *item) {
    if (@available(macOS 26.0, *)) return bounded(item.addressRepresentations.cityName, 256);
    return bounded(item.placemark.locality, 256);
}
static NSString *publicCountry(MKMapItem *item) {
    if (@available(macOS 26.0, *)) return bounded(item.addressRepresentations.regionCode, 16);
    return bounded(item.placemark.ISOcountryCode, 16);
}
#pragma clang diagnostic pop

static NSDictionary *listing(MKMapItem *item) {
    if (item.isCurrentLocation) return nil;
    NSString *name = bounded(item.name, 512);
    if (!name) return nil;
    NSString *identifier = bounded(item.identifier.identifierString, 256);
    NSString *mapURL = nil;
    if (identifier) {
        NSURLComponents *url = [[NSURLComponents alloc] init];
        url.scheme = @"https";
        url.host = @"maps.apple.com";
        url.path = @"/place";
        url.queryItems = @[[NSURLQueryItem queryItemWithName:@"place-id" value:identifier]];
        mapURL = bounded(url.URL.absoluteString, 2048);
    }
    // item.url is the business website, not an Apple listing URL. Never derive
    // verification from it, arbitrary input URL text, or a user-created map item.
    return @{@"name": name,
        @"address": publicAddress(item) ?: [NSNull null],
        @"city": publicCity(item) ?: [NSNull null],
        @"country_code": publicCountry(item) ?: [NSNull null],
        @"phone": bounded(item.phoneNumber, 128) ?: [NSNull null],
        @"place_id": identifier ?: [NSNull null],
        @"map_url": mapURL ?: [NSNull null]};
}

static int lookup_query(NSString *query, uint32_t timeoutMs,
                        uint8_t **output, size_t *outputLen) {
    if (!output || !outputLen) return 64;
    *output = NULL;
    *outputLen = 0;
    @autoreleasepool {
        if (![NSThread isMainThread])
            return emit(envelope(@"error", @[], NO, @"main_thread_required"), output, outputLen, 70);
        if (!query || timeoutMs == 0 || timeoutMs > 15000)
            return emit(envelope(@"invalid_input", @[], NO, @"input_bounds"), output, outputLen, 64);
        @try {
            MKLocalSearchRequest *request = [[MKLocalSearchRequest alloc] init];
            request.naturalLanguageQuery = query;
            request.resultTypes = MKLocalSearchResultTypePointOfInterest;
            MKLocalSearch *search = [[MKLocalSearch alloc] initWithRequest:request];
            __block BOOL finished = NO;
            __block NSDictionary *result = nil;
            __block int resultCode = 0;
            [search startWithCompletionHandler:^(MKLocalSearchResponse *response, NSError *error) {
                // Serialize all callback state on the main queue. This is safe
                // even if a future OS invokes the MapKit callback elsewhere.
                dispatch_async(dispatch_get_main_queue(), ^{
                    if (finished) return;
                    if (error || !response) {
                        result = envelope(@"error", @[], NO,
                            error ? [NSString stringWithFormat:@"mapkit_%ld", (long)error.code] : @"response_missing");
                        resultCode = 69;
                    } else {
                        NSMutableArray *items = [NSMutableArray array];
                        BOOL truncated = response.mapItems.count > MAX_RESULTS;
                        NSUInteger examined = 0;
                        for (MKMapItem *item in response.mapItems) {
                            if (examined++ >= MAX_RESULTS) break;
                            NSDictionary *record = listing(item);
                            if (record) [items addObject:record];
                            else truncated = YES;
                        }
                        result = envelope(items.count ? @"ok" : @"no_results", items, truncated, nil);
                    }
                    finished = YES;
                });
            }];
            NSTimeInterval deadline = NSProcessInfo.processInfo.systemUptime + timeoutMs / 1000.0;
            while (!finished && NSProcessInfo.processInfo.systemUptime < deadline) {
                @autoreleasepool {
                    [NSRunLoop.mainRunLoop runMode:NSDefaultRunLoopMode
                        beforeDate:[NSDate dateWithTimeIntervalSinceNow:0.025]];
                }
            }
            if (!finished) {
                finished = YES;
                [search cancel];
                result = envelope(@"timeout", @[], NO, @"lookup_deadline");
                resultCode = 75;
            }
            return emit(result, output, outputLen, resultCode);
        } @catch (NSException *exception) {
            (void)exception;
            return emit(envelope(@"error", @[], NO, @"native_exception"), output, outputLen, 70);
        }
    }
}

int maps_lookup_json(const uint8_t *nameBytes, size_t nameLen,
                     const uint8_t *localityBytes, size_t localityLen,
                     uint32_t timeoutMs, uint8_t **output, size_t *outputLen) {
    if (!output || !outputLen) return 64;
    *output = NULL;
    *outputLen = 0;
    @autoreleasepool {
        if (!nameBytes || !localityBytes || nameLen == 0 || nameLen > 256 ||
            localityLen == 0 || localityLen > 256)
            return emit(envelope(@"invalid_input", @[], NO, @"input_bounds"), output, outputLen, 64);
        NSString *name = [[NSString alloc] initWithBytes:nameBytes length:nameLen encoding:NSUTF8StringEncoding];
        NSString *locality = [[NSString alloc] initWithBytes:localityBytes length:localityLen encoding:NSUTF8StringEncoding];
        NSCharacterSet *controls = [NSCharacterSet controlCharacterSet];
        if (!name || !locality ||
            [name rangeOfCharacterFromSet:controls].location != NSNotFound ||
            [locality rangeOfCharacterFromSet:controls].location != NSNotFound ||
            [name stringByTrimmingCharactersInSet:NSCharacterSet.whitespaceAndNewlineCharacterSet].length == 0 ||
            [locality stringByTrimmingCharactersInSet:NSCharacterSet.whitespaceAndNewlineCharacterSet].length == 0)
            return emit(envelope(@"invalid_input", @[], NO, @"input_text"), output, outputLen, 64);
        return lookup_query([NSString stringWithFormat:@"%@, %@", name, locality],
                            timeoutMs, output, outputLen);
    }
}

int maps_lookup_phone_json(const uint8_t *phoneBytes, size_t phoneLen,
                           uint32_t timeoutMs, uint8_t **output, size_t *outputLen) {
    if (!output || !outputLen) return 64;
    *output = NULL;
    *outputLen = 0;
    @autoreleasepool {
        // Defend the native boundary too: no alternate spelling, model text,
        // fabricated locality, URI or current-location hint enters this path.
        if (!phoneBytes || phoneLen < 3 || phoneLen > 16 || phoneBytes[0] != '+' ||
            phoneBytes[1] < '1' || phoneBytes[1] > '9')
            return emit(envelope(@"invalid_input", @[], NO, @"input_phone"), output, outputLen, 64);
        for (size_t index = 2; index < phoneLen; index++) {
            if (phoneBytes[index] < '0' || phoneBytes[index] > '9')
                return emit(envelope(@"invalid_input", @[], NO, @"input_phone"), output, outputLen, 64);
        }
        NSString *phone = [[NSString alloc] initWithBytes:phoneBytes length:phoneLen encoding:NSUTF8StringEncoding];
        return lookup_query(phone, timeoutMs, output, outputLen);
    }
}

void maps_lookup_free(uint8_t *output) { free(output); }
