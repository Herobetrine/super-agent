/* Isolated Claude/Codex CLI fixture. Protocol and discovery stay local. */
#ifndef _WIN32
#define _POSIX_C_SOURCE 200809L
#endif
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef _WIN32
#include <windows.h>
#else
#include <time.h>
#endif

#define MODEL "fixture-model@20250101"

static void delay_preflight(void) {
#ifdef _WIN32
    Sleep(150);
#else
    const struct timespec delay = {0, 150000000};
    nanosleep(&delay, NULL);
#endif
}

static int has_arg(int argc, char **argv, const char *arg) {
    for (int i = 1; i < argc; ++i) {
        if (strcmp(argv[i], arg) == 0) return 1;
    }
    return 0;
}

static int receipt(const char *kind) {
    const char *path = getenv("SA_STARTUP_FIXTURE_LOG");
    FILE *log = path ? fopen(path, "a") : NULL;
    if (!log) return 0;
    int written = fputs(kind, log) >= 0 && fputc('\n', log) != EOF;
    return fclose(log) == 0 && written;
}

static int fail_turn(void) {
    FILE *marker = fopen("fail-turn", "r");
    if (!marker) return 0;
    fclose(marker);
    return 1;
}

static int codex_turn(void) {
    char line[16384];
    while (fgets(line, sizeof(line), stdin)) {
        if (strstr(line, "\"method\":\"initialize\"")) {
            puts("{\"id\":1,\"result\":{}}");
        } else if (strstr(line, "\"method\":\"thread/start\"")) {
            const char *provider = "openai";
            const char *sandbox = strstr(line, "\"sandbox\":\"read-only\"") ? "readOnly" : "dangerFullAccess";
            if (strstr(line, "\"modelProvider\":\"provider-a\"")) provider = "provider-a";
            if (strstr(line, "\"modelProvider\":\"provider-b\"")) provider = "provider-b";
            printf("{\"id\":2,\"result\":{\"sandbox\":{\"type\":\"%s\"},"
                   "\"approvalPolicy\":\"never\",\"model\":\"" MODEL "\","
                   "\"modelProvider\":\"%s\",\"thread\":{\"id\":\"fixture-session\"}}}\n", sandbox, provider);
        } else if (strstr(line, "\"method\":\"turn/start\"")) {
            if (fail_turn()) {
                puts("{\"id\":3,\"error\":{\"code\":-32000,\"message\":\"fixture failure\"}}");
                return 0;
            }
            puts("{\"id\":3,\"result\":{\"turn\":{\"id\":\"fixture-turn\"}}}");
            puts("{\"method\":\"item/completed\",\"params\":{\"threadId\":\"fixture-session\","
                 "\"item\":{\"id\":\"answer\",\"type\":\"agentMessage\",\"text\":\"fixture-ok\"}}}");
            puts("{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"fixture-session\","
                 "\"turn\":{\"id\":\"fixture-turn\",\"status\":\"completed\"}}}");
            return 0;
        }
        fflush(stdout);
    }
    return 2;
}

int main(int argc, char **argv) {
    if (has_arg(argc, argv, "mcp") && has_arg(argc, argv, "list")) {
        if (!receipt("mcp-list")) return 3;
        delay_preflight();
        puts("[]");
        return 0;
    }
    if (has_arg(argc, argv, "--version")) {
        if (!receipt("claude-version")) return 3;
        delay_preflight();
        puts("2.1.242");
        return 0;
    }
    if (has_arg(argc, argv, "debug")) {
        if (!receipt("codex-models")) return 3;
        puts("{\"models\":[{\"slug\":\"" MODEL "\",\"display_name\":\"Fixture\","
             "\"reasoningEfforts\":[\"low\"],\"capabilities\":{\"private\":\"fixture-secret\"}}]}");
        return 0;
    }
    if (has_arg(argc, argv, "auth")) {
        if (!receipt("claude-auth")) return 3;
        puts("{\"loggedIn\":true,\"apiProvider\":\"firstParty\",\"email\":\"fixture@example.com\"}");
        return 0;
    }
    if (has_arg(argc, argv, "app-server")) return codex_turn();

    char line[16384];
    if (!fgets(line, sizeof(line), stdin)) return 2;
    if (strstr(line, "\"subtype\":\"initialize\"")) {
        if (!receipt("claude-models")) return 3;
        puts("{\"response\":{\"request_id\":\"catalog\",\"subtype\":\"success\","
             "\"response\":{\"models\":[{\"value\":\"" MODEL "\",\"label\":\"Fixture\","
             "\"reasoningEfforts\":[\"low\"],\"capabilities\":{\"private\":\"fixture-secret\"}}]}}}");
        return 0;
    }
    puts("{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"fixture-session\",\"model\":\"" MODEL "\"}");
    if (fail_turn()) {
        puts("{\"type\":\"result\",\"subtype\":\"error\",\"session_id\":\"fixture-session\",\"is_error\":true}");
    } else {
        puts("{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"fixture-session\",\"result\":\"fixture-ok\"}");
    }
    return 0;
}
