FROM python:3.12-slim

WORKDIR /app

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY . .

ENV ATLAS_HOME = /app
ENV PYTHONUNBUFFERED = 1

EXPOSE 8080

VOLUME ["/app/data"]

ENTRYPOINT ["python", "main.py"]
