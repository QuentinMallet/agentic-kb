defmodule AgenticKbMcp.JsonRpc do
  @moduledoc false

  @invalid_request -32_600
  @invalid_params -32_602

  @type classification ::
          {:request, String.t(), String.t() | integer(), map()}
          | {:notification, String.t(), map()}
          | {:error, map()}

  @spec classify(term()) :: classification()
  def classify(%{"jsonrpc" => "2.0", "method" => method} = request) when is_binary(method) do
    params = Map.get(request, "params", %{})

    cond do
      not is_map(params) ->
        {:error, invalid_request()}

      Map.has_key?(request, "id") and not valid_id?(request["id"]) ->
        {:error, invalid_request()}

      Map.has_key?(request, "id") ->
        {:request, method, request["id"], params}

      true ->
        {:notification, method, params}
    end
  end

  def classify(_request), do: {:error, invalid_request()}

  @spec validate_method(map()) ::
          {:ok, String.t(), String.t() | integer(), map()} | {:error, map()}
  def validate_method(%{"jsonrpc" => "2.0", "method" => "tools/call", "id" => id} = request) do
    params = Map.get(request, "params", %{})

    if is_binary(params["name"]) do
      {:ok, "tools/call", id, params}
    else
      {:error, invalid_params(id, "tools/call requires a string params.name")}
    end
  end

  def validate_method(%{"jsonrpc" => "2.0", "method" => method, "id" => id} = request)
      when is_binary(method) do
    {:ok, method, id, Map.get(request, "params", %{})}
  end

  defp valid_id?(id), do: is_binary(id) or is_integer(id)

  def invalid_request do
    error(:null, @invalid_request, "Invalid Request")
  end

  def invalid_params(id, message) do
    error(id, @invalid_params, "Invalid params: #{message}")
  end

  defp error(id, code, message) do
    %{"jsonrpc" => "2.0", "id" => id, "error" => %{"code" => code, "message" => message}}
  end
end
