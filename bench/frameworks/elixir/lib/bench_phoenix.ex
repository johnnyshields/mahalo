defmodule BenchPhoenix.Application do
  use Application

  @impl true
  def start(_type, _args) do
    port = String.to_integer(System.get_env("PORT") || "3007")

    children = [
      {Plug.Cowboy, scheme: :http, plug: BenchPhoenix.Router, options: [port: port]}
    ]

    IO.puts("Plug/Cowboy listening on 0.0.0.0:#{port}")
    Supervisor.start_link(children, strategy: :one_for_one)
  end
end

defmodule BenchPhoenix.Router do
  use Plug.Router

  plug :match
  plug :dispatch

  get "/plaintext" do
    conn
    |> put_resp_content_type("text/plain")
    |> send_resp(200, "Hello, World!")
  end

  get "/json" do
    conn
    |> put_resp_content_type("application/json")
    |> send_resp(200, Jason.encode!(%{message: "Hello, World!"}))
  end

  match _ do
    send_resp(conn, 404, "Not Found")
  end
end
