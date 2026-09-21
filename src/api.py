from flask import Flask, Response, request
from threading import Thread


app = Flask(__name__)


@app.get("/api")
def api():
    
    if request.args.get("t") != "caps":
        return Response("", status = 404)

    xml = """<?xml version="1.0" encoding="UTF-8"?>
<caps xmlns="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <server title="Atlas" version="1.0" url="http://127.0.0.1:8080/api" />
  <limits max="100" default="100" />
</caps>"""
    
    return Response(xml, mimetype = "application/xml")


def start(config):
    port = int(config.get("api_port", 9090))
    
    thread = Thread(
        target = app.run,
        kwargs = {"host": "127.0.0.1", "port": port, "use_reloader": False},
        daemon = True,
    )
  
    thread.start()

    return thread