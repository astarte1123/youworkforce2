# Go console — multi-stage. Serves ./static and reverse-proxies /api to core.
FROM golang:1.25-bookworm AS build
WORKDIR /src
COPY go.mod ./
COPY main.go ./
RUN CGO_ENABLED=0 go build -o /out/console .

FROM debian:bookworm-slim
WORKDIR /app
COPY --from=build /out/console /app/console
COPY static /app/static
EXPOSE 8081
CMD ["/app/console"]
