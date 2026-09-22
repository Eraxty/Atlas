FROM python:3.12-slim

WORKDIR /app

COPY requirements.txt .
RUN grep -v "SABnzbd" requirements.txt > requirements.container.txt && pip install --no-cache-dir -r requirements.container.txt

COPY . .

ENV ATLAS_HOME=/app/data
ENV PYTHONUNBUFFERED=1

VOLUME ["/app/data"]

ENTRYPOINT ["python", "main.py"]
