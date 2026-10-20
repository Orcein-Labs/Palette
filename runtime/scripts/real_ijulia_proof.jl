# Drive a real IJulia kernel over the Jupyter wire protocol (ZMQ, signed
# messages), the way a notebook client does, and check that state persists
# between execute requests: x = 41, then x + 1 == 42; then Palette loaded
# inside the kernel keeps its own state between two of its calls.
#
# The environment needs Palette (developed from this checkout), IJulia, ZMQ
# and JSON, and an IJulia kernelspec named PaletteTest installed into an
# isolated Jupyter data directory:
#
#   julia --project=ijulia-env -e 'using Pkg; Pkg.develop(path="."); Pkg.add(["IJulia", "ZMQ", "JSON"])'
#   JUPYTER_DATA_DIR=jupyter-data julia --project=ijulia-env -e '
#     using IJulia; IJulia.installkernel("PaletteTest", "--project=$(abspath("ijulia-env"))")'
#
# Run:
#
#   JUPYTER_DATA_DIR=jupyter-data julia --project=ijulia-env runtime/scripts/real_ijulia_proof.jl

using Dates, JSON, SHA, Sockets, UUIDs, ZMQ

const DELIMITER = "<IDS|MSG>"

function kernel_argv(data_dir)
    kernels = joinpath(data_dir, "kernels")
    names = filter(startswith("palettetest"), readdir(kernels))
    length(names) == 1 || error("expected one palettetest kernelspec in $kernels, found $names")
    JSON.parse(read(joinpath(kernels, only(names), "kernel.json"), String))["argv"]
end

function free_port()
    server = listen(ip"127.0.0.1", 0)
    port = Int(getsockname(server)[2])
    close(server)
    port
end

sign(key, parts) = bytes2hex(hmac_sha256(Vector{UInt8}(key), Vector{UInt8}(join(parts))))

struct Client
    key::String
    session::String
    shell::Socket
    messages::Dict{Symbol,Channel{Dict{String,Any}}}
end

function send_request(client, msg_type, content)
    header = Dict("msg_id" => string(uuid4()), "session" => client.session, "username" => "palette-proof",
                  "date" => string(now(UTC)) * "Z", "msg_type" => msg_type, "version" => "5.3")
    parts = [JSON.json(header), "{}", "{}", JSON.json(content)]
    ZMQ.send_multipart(client.shell, [DELIMITER, sign(client.key, parts), parts...])
    header["msg_id"]
end

# Each socket's messages, with their signatures checked, on a channel.
function receive(key, socket)
    Channel{Dict{String,Any}}(Inf) do channel
        while true
            frames = ZMQ.recv_multipart(socket, String)
            at = findfirst(==(DELIMITER), frames)
            signature, parts = frames[at+1], frames[at+2:at+5]
            signature == sign(key, parts) || error("a kernel message has a bad signature")
            header, parent, _, content = JSON.parse.(parts)
            put!(channel, Dict{String,Any}("header" => header, "parent" => parent, "content" => content))
        end
    end
end

function next_message(channel, seconds)
    timedwait(() -> isready(channel), seconds) === :ok || error("no kernel message within $seconds s")
    take!(channel)
end

replies_to(message, id) = get(message["parent"], "msg_id", nothing) == id

function wait_for(channel, id, msg_type, seconds)
    while true
        message = next_message(channel, seconds)
        replies_to(message, id) && message["header"]["msg_type"] == msg_type && return message
    end
end

# The kernel answers kernel_info on shell; it is ready once IOPub, which may
# connect later than shell, also carries the status of a request.
function wait_until_ready(client, seconds)
    deadline = time() + seconds
    while time() < deadline
        id = send_request(client, "kernel_info_request", Dict())
        wait_for(client.messages[:shell], id, "kernel_info_reply", seconds)
        iopub = client.messages[:iopub]
        while timedwait(() -> isready(iopub), 2) === :ok
            replies_to(take!(iopub), id) && return
        end
    end
    error("the kernel was not ready within $seconds s")
end

# The text/plain results a cell produced, and whether its reply was ok.
function run_cell(client, code; seconds = 300)
    id = send_request(client, "execute_request", Dict("code" => code, "silent" => false, "store_history" => true,
                      "user_expressions" => Dict(), "allow_stdin" => false, "stop_on_error" => true))
    results = String[]
    while true
        message = next_message(client.messages[:iopub], seconds)
        replies_to(message, id) || continue
        msg_type, content = message["header"]["msg_type"], message["content"]
        msg_type == "execute_result" && push!(results, content["data"]["text/plain"])
        msg_type == "error" && println(join(content["traceback"], "\n"))
        msg_type == "status" && content["execution_state"] == "idle" && break
    end
    reply = wait_for(client.messages[:shell], id, "execute_reply", seconds)
    reply["content"]["status"] == "ok", results
end

function main()
    argv = kernel_argv(ENV["JUPYTER_DATA_DIR"])
    key = string(uuid4())
    ports = Dict(name => free_port() for name in ("shell", "iopub", "stdin", "control", "hb"))
    connection = Dict("ip" => "127.0.0.1", "transport" => "tcp", "key" => key, "signature_scheme" => "hmac-sha256",
                      "kernel_name" => "palettetest", (name * "_port" => port for (name, port) in ports)...)
    dir = mktempdir()
    connection_file = joinpath(dir, "kernel.json")
    write(connection_file, JSON.json(connection))
    log = joinpath(dir, "kernel.log")
    kernel = run(pipeline(Cmd(replace.(argv, "{connection_file}" => connection_file)); stdout = log, stderr = log); wait = false)
    println("[real-ijulia-proof] started an IJulia kernel: ", join(argv, " "))

    context = Context()
    shell = Socket(context, DEALER)
    iopub = Socket(context, SUB)
    subscribe(iopub, "")
    connect(shell, "tcp://127.0.0.1:$(ports["shell"])")
    connect(iopub, "tcp://127.0.0.1:$(ports["iopub"])")
    client = Client(key, string(uuid4()), shell, Dict(:shell => receive(key, shell), :iopub => receive(key, iopub)))

    checks = Pair{String,Bool}[]
    try
        wait_until_ready(client, 300)
        println("[real-ijulia-proof] kernel ready over ZMQ")

        ok, _ = run_cell(client, "x = 41")
        println("[req 1] x = 41 -> ok=$ok")
        push!(checks, "req1" => ok)

        ok, results = run_cell(client, "x + 1")
        println("[req 2] x + 1 -> ok=$ok results=$results")
        push!(checks, "req2_is_42" => ok && "42" in results)

        ok, _ = run_cell(client, """using Palette; Palette.reset_kernel_state(); Palette.execute(Palette.ExecuteCode("y = 10"))""")
        println("[req 3] load Palette, execute through it -> ok=$ok")
        push!(checks, "req3_package_loads_in_real_kernel" => ok)

        ok, results = run_cell(client, """Palette.execute(Palette.ExecuteCode("y + 5")).result.data""")
        println("[req 4] Palette's own persistence inside the kernel -> ok=$ok results=$results")
        push!(checks, "req4_palette_persistence_in_real_kernel" => ok && "15" in results)
    catch
        print(read(log, String))
        rethrow()
    finally
        kill(kernel, Base.SIGKILL)
        wait(kernel)
        close(shell)
        close(iopub)
        close(context)
        rm(dir; recursive = true, force = true)
    end

    println("\n=== SUMMARY ===")
    for (name, passed) in checks
        println(name, ": ", passed ? "PASS" : "FAIL")
    end
    exit(length(checks) == 4 && all(last, checks) ? 0 : 1)
end

main()
