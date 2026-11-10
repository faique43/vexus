"""The 14 short change requests and how each is scored."""
import os
import re
import subprocess

EXPRESS_REF = "v5.3.0"
NEST_REF = "8d9dd9d174921cc03882d1b39e91fb7bb1b1ca5a"

EXPRESS = {
    "E1": "Add a res.noStore() method to Express responses. It should set the Cache-Control header to no-store and return res so calls can be chained.",
    "E2": "Add a req.bearerToken getter that returns the token from an `Authorization: Bearer <token>` header. The scheme is case-insensitive. Return undefined when the header is missing or uses another scheme.",
    "E3": "Add an app setting called `json sort keys`. When it is enabled, res.json must serialize objects with their keys sorted at every depth, including objects inside arrays. It is off by default.",
    "E4": "res.download should accept an `inline: true` option that sends the file with a Content-Disposition of inline instead of attachment, keeping the filename.",
    "E5": "Change res.redirect so it throws a TypeError when the url is not a non-empty string, instead of only logging a deprecation warning. res.redirect(301, '/x') must keep working.",
    "E6": "Add an app setting called `default cookie options`: an object merged into the options of every res.cookie() and res.clearCookie() call. Options passed explicitly to the call win over the defaults.",
    "E7": "Conditional GETs are broken. A client sends If-None-Match with the ETag it was given, for a plain 200 response, and still gets a 200 with the full body instead of a 304. Find the cause and fix it.",
}

NEST = {
    "N1": "Add a ParseHexPipe to @nestjs/common. It works like ParseIntPipe but parses hexadecimal strings such as '0xff' or 'ff' into numbers. Export it publicly the same way the other pipes are exported.",
    "N2": "Add a @UserAgent() route parameter decorator to @nestjs/common that injects the request's User-Agent header into a controller method parameter. Export it like the other HTTP parameter decorators.",
    "N3": "Add a `dateFormatter?: (date: Date) => string` option to the ConsoleLogger options and use it when the logger formats the date in each log line. Keep the current format when it isn't set.",
    "N4": "When Nest can't resolve a dependency, append this line to the error message: `Hint: run with NEST_DEBUG=true to print the injector trace.`",
    "N5": "At startup, when two methods of one controller map to the same HTTP method and path, log a warning naming both handlers.",
    "N6": "Add a `connectTimeout` option to the TCP microservice client: if the connection isn't established within that many milliseconds, connect() must reject with a timeout error. Add it to the client's options type too.",
    "N7": "Validation bug: a request to /users/12abc with @Param('id', ParseIntPipe) reaches the handler instead of returning a 400. Find the cause and fix it.",
}

# E7 and N7 start from a seeded bug, described to the agent only by its symptom.
SEEDS = {
    "express": [(
        "lib/request.js",
        "(status >= 200 && status < 300) || 304 === status",
        "(status > 200 && status < 300) || 304 === status",
    )],
    "nest": [(
        "packages/common/pipes/parse-int.pipe.ts",
        "/^-?\\d+$/.test(String(value)) &&\n      isFinite(value as any)",
        "/^-?\\d+/.test(String(value)) &&\n      isFinite(parseInt(String(value), 10))",
    )],
}


# The existing files a fix changes, for scoring the hook's hints offline. Taken
# from passing sessions: every file that more than half of them changed. A task
# that adds a file lists only what it edits, since a hint can't point at a file
# that doesn't exist yet.
FILES = {
    "E1": ["lib/response.js"],
    "E2": ["lib/request.js"],
    "E3": ["lib/response.js"],
    "E4": ["lib/response.js"],
    "E5": ["lib/response.js"],
    "E6": ["lib/response.js"],
    "E7": ["lib/request.js"],
    "N1": ["packages/common/pipes/index.ts"],
    "N2": ["packages/common/decorators/http/route-params.decorator.ts"],
    "N3": ["packages/common/services/console-logger.service.ts"],
    "N4": ["packages/core/errors/messages.ts"],
    "N5": ["packages/core/router/router-explorer.ts"],
    "N6": ["packages/microservices/client/client-tcp.ts", "packages/microservices/interfaces/client-metadata.interface.ts"],
    "N7": ["packages/common/pipes/parse-int.pipe.ts"],
}


def changed_files(work_dir):
    out = subprocess.run(["git", "status", "--porcelain", "-uall"], cwd=work_dir, capture_output=True, text=True).stdout
    return [line[3:].strip().strip('"') for line in out.splitlines()]


def passes_nest(work_dir, task):
    """NestJS has no hidden test here, so these check the change landed. A valid
    fix elsewhere reads as a miss, so they undercount and never overcount."""
    files = changed_files(work_dir)

    def read(path):
        full = f"{work_dir}/{path}"
        return open(full, errors="replace").read() if os.path.isfile(full) else ""

    def has(path, pattern):
        return bool(re.search(pattern, read(path)))

    int_pipe = "packages/common/pipes/parse-int.pipe.ts"
    return {
        "N1": lambda: any(f.startswith("packages/common/pipes/") and "hex" in f.lower() for f in files)
        and has("packages/common/pipes/index.ts", "hex"),
        "N2": lambda: any("decorators/http" in f and has(f, r"export (function|const) UserAgent") for f in files),
        "N3": lambda: len(re.findall("dateFormatter", read("packages/common/services/console-logger.service.ts"))) >= 2,
        "N4": lambda: any(has(f, "NEST_DEBUG=true") for f in files if f.startswith("packages/core/")),
        "N5": lambda: any(f.startswith("packages/core/") and has(f, r"(\bwarn\(|\.warn|'warn')") for f in files),
        "N6": lambda: has("packages/microservices/client/client-tcp.ts", "connectTimeout")
        and has("packages/microservices/interfaces/client-metadata.interface.ts", "connectTimeout"),
        "N7": lambda: not (has(int_pipe, r"\^-\?\\d\+/") and has(int_pipe, r"isFinite\(parseInt")),
    }[task]()


def passes_express(work_dir, task):
    here = os.path.dirname(os.path.abspath(__file__))
    try:
        run = subprocess.run(["node", f"{here}/check_express.js", work_dir, task], capture_output=True, text=True, timeout=20)
    except subprocess.TimeoutExpired:
        return False
    return run.stdout.strip().startswith("PASS")
