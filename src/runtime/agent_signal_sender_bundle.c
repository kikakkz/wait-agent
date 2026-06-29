#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

struct buffer {
  char *data;
  size_t len;
  size_t cap;
};

static int buf_reserve(struct buffer *buf, size_t extra) {
  if (buf->len + extra + 1 <= buf->cap) {
    return 0;
  }
  size_t next_cap = buf->cap ? buf->cap : 4096;
  while (buf->len + extra + 1 > next_cap) {
    next_cap *= 2;
  }
  char *next = (char *)realloc(buf->data, next_cap);
  if (!next) {
    return -1;
  }
  buf->data = next;
  buf->cap = next_cap;
  return 0;
}

static int buf_append_len(struct buffer *buf, const char *value, size_t len) {
  if (buf_reserve(buf, len) != 0) {
    return -1;
  }
  memcpy(buf->data + buf->len, value, len);
  buf->len += len;
  buf->data[buf->len] = '\0';
  return 0;
}

static int buf_append(struct buffer *buf, const char *value) {
  return buf_append_len(buf, value, strlen(value));
}

static int buf_append_char(struct buffer *buf, char value) {
  if (buf_reserve(buf, 1) != 0) {
    return -1;
  }
  buf->data[buf->len++] = value;
  buf->data[buf->len] = '\0';
  return 0;
}

static int json_append_string(struct buffer *out, const char *value) {
  if (buf_append_char(out, '"') != 0) {
    return -1;
  }
  for (const unsigned char *p = (const unsigned char *)value; *p; p++) {
    switch (*p) {
    case '"':
      if (buf_append(out, "\\\"") != 0)
        return -1;
      break;
    case '\\':
      if (buf_append(out, "\\\\") != 0)
        return -1;
      break;
    case '\n':
      if (buf_append(out, "\\n") != 0)
        return -1;
      break;
    case '\r':
      if (buf_append(out, "\\r") != 0)
        return -1;
      break;
    case '\t':
      if (buf_append(out, "\\t") != 0)
        return -1;
      break;
    default:
      if (*p < 0x20) {
        char escaped[7];
        snprintf(escaped, sizeof(escaped), "\\u%04x", *p);
        if (buf_append(out, escaped) != 0)
          return -1;
      } else {
        if (buf_append_char(out, (char)*p) != 0)
          return -1;
      }
    }
  }
  return buf_append_char(out, '"');
}

static char *read_stdin(void) {
  size_t cap = 4096;
  size_t len = 0;
  char *buf = (char *)malloc(cap);
  if (!buf) {
    return NULL;
  }
  for (;;) {
    if (len + 2048 + 1 > cap) {
      cap *= 2;
      char *next = (char *)realloc(buf, cap);
      if (!next) {
        free(buf);
        return NULL;
      }
      buf = next;
    }
    size_t n = fread(buf + len, 1, 2048, stdin);
    len += n;
    if (n < 2048) {
      if (ferror(stdin)) {
        free(buf);
        return NULL;
      }
      break;
    }
  }
  buf[len] = '\0';
  return buf;
}

static int blank(const char *s) {
  if (!s || !*s) {
    return 1;
  }
  for (; *s; s++) {
    if (*s != ' ' && *s != '\n' && *s != '\r' && *s != '\t') {
      return 0;
    }
  }
  return 1;
}

int main(int argc, char **argv) {
  const char *event = argc > 1 ? argv[1] : "";
  const char *signal_socket = getenv("WAITAGENT_SIGNAL_SOCKET");
  const char *socket_name = getenv("WAITAGENT_SOCKET_NAME");
  const char *session = getenv("WAITAGENT_TARGET_SESSION_NAME");
  const char *pane = getenv("WAITAGENT_PANE_ID");
  const char *token = getenv("WAITAGENT_AGENT_SIGNAL_TOKEN");
  const char *agent = getenv("WAITAGENT_AGENT_NAME");
  if (!agent || !*agent) {
    agent = "codex";
  }

  if (!event || !*event || !agent || !*agent || !signal_socket || !*signal_socket || !socket_name ||
      !*socket_name || !session || !*session || !pane || !*pane || !token ||
      !*token) {
    return 0;
  }

  char *payload = read_stdin();
  if (!payload) {
    return 0;
  }

  struct buffer message = {0};
  if (buf_append(&message, "{\"version\":1,\"agent\":") != 0 ||
      json_append_string(&message, agent) != 0 ||
      buf_append(&message, ",\"event\":") !=
      0 ||
      json_append_string(&message, event) != 0 ||
      buf_append(&message, ",\"socket\":") != 0 ||
      json_append_string(&message, socket_name) != 0 ||
      buf_append(&message, ",\"session\":") != 0 ||
      json_append_string(&message, session) != 0 ||
      buf_append(&message, ",\"pane\":") != 0 ||
      json_append_string(&message, pane) != 0 ||
      buf_append(&message, ",\"token\":") != 0 ||
      json_append_string(&message, token) != 0 ||
      buf_append(&message, ",\"payload\":") != 0 ||
      buf_append(&message, blank(payload) ? "null" : payload) != 0 ||
      buf_append_char(&message, '}') != 0) {
    free(payload);
    free(message.data);
    return 0;
  }
  free(payload);
  if (message.len > 65535) {
    free(message.data);
    return 0;
  }

  int sock = socket(AF_UNIX, SOCK_DGRAM, 0);
  if (sock >= 0) {
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, signal_socket, sizeof(addr.sun_path) - 1);
    sendto(sock, message.data, message.len, 0, (struct sockaddr *)&addr,
           sizeof(addr));
    close(sock);
  }
  free(message.data);
  return 0;
}
