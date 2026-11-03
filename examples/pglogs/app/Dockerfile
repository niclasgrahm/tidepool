FROM python:3.12-slim
RUN pip install --no-cache-dir "psycopg[binary]==3.2.3"
COPY app.py /app/app.py
CMD ["python", "-u", "/app/app.py"]
